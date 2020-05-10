// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use bytes::Bytes;
use std::convert::TryFrom;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time;

use futures::channel::mpsc;
use futures::future::{self, Either};
use futures::prelude::*;
use futures::select;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_util::codec::FramedParts;

use ii_async_utils::FutureExt;
use ii_logging::macros::*;
use ii_stratum::v1;
use ii_stratum::v2;
use ii_wire::{
    proxy::{Acceptor, Connector, ProxyStream, WithProxyInfo},
    Address, Client, Connection, Server,
};

use crate::error::{Error, Result};
use crate::translation::V2ToV1Translation;

/// Represents a single protocol translation session (one V2 client talking to one V1 server)
pub struct ConnTranslation {
    /// Actual protocol translator
    translation: V2ToV1Translation,
    /// Upstream connection
    v1_conn: v1::Framed,
    /// Address of the v1 upstream peer
    v1_peer_addr: SocketAddr,
    // TODO to be removed as the translator may send out items directly via a particular connection
    // (when treated as a sink)
    /// Frames from the translator to be sent out via V1 connection
    v1_translation_rx: mpsc::Receiver<v1::Frame>,
    /// Downstream connection
    v2_conn: v2::Framed,
    /// Address of the v2 peer that has connected
    v2_peer_addr: SocketAddr,
    /// Frames from the translator to be sent out via V2 connection
    v2_translation_rx: mpsc::Receiver<v2::Frame>,
}

impl ConnTranslation {
    const MAX_TRANSLATION_CHANNEL_SIZE: usize = 10;
    const V1_UPSTREAM_TIMEOUT: time::Duration = time::Duration::from_secs(60);
    const V2_DOWNSTREAM_TIMEOUT: time::Duration = time::Duration::from_secs(60);

    fn new(
        v2_conn: v2::Framed,
        v2_peer_addr: SocketAddr,
        v1_conn: v1::Framed,
        v1_peer_addr: SocketAddr,
    ) -> Self {
        let (v1_translation_tx, v1_translation_rx) =
            mpsc::channel(Self::MAX_TRANSLATION_CHANNEL_SIZE);
        let (v2_translation_tx, v2_translation_rx) =
            mpsc::channel(Self::MAX_TRANSLATION_CHANNEL_SIZE);
        let translation =
            V2ToV1Translation::new(v1_translation_tx, v2_translation_tx, Default::default());

        Self {
            translation,
            v1_conn,
            v1_peer_addr,
            v1_translation_rx,
            v2_conn,
            v2_peer_addr,
            v2_translation_rx,
        }
    }

    async fn v1_handle_frame(
        translation: &mut V2ToV1Translation,
        frame: v1::framing::Frame,
    ) -> Result<()> {
        let deserialized = v1::rpc::Rpc::try_from(frame)?;
        translation.handle_v1(deserialized).await
    }

    //    async fn handle_frame(&mut self, frame: v2::framing::Frame) -> Result<()> {
    async fn v2_handle_frame(
        translation: &mut V2ToV1Translation,
        frame: v2::framing::Frame,
    ) -> Result<()> {
        match frame.header.extension_type {
            v2::extensions::BASE => {
                translation.handle_v2(frame).await?;
            }
            // Report any other extension down the line
            _ => {
                warn!("Unsupported extension frame: {:x?} ", frame);
            }
        }
        Ok(())
    }

    /// Attempt to send a frame via a specified connection. Attempt to send 'None' results in an
    /// error. The intention is to have a single place for sending out frames and handling
    /// errors/timeouts.
    pub async fn v2_try_send_frame<S>(
        connection: &mut S,
        frame: Option<v2::framing::Frame>,
        peer_addr: &SocketAddr,
    ) -> Result<()>
    where
        S: v2::FramedSink,
    {
        let status = match frame {
            Some(v2_translated_frame) => connection.send(v2_translated_frame).await,
            None => Err(Error::Io(io::Error::new(
                io::ErrorKind::Other,
                "No more frames".to_string(),
            )))?,
        };
        status.map_err(|e| {
            info!("Send error: {} for (peer: {:?})", e, peer_addr);
            e.into()
        })
    }

    /// Send all V2 frames via the specified V2 connection
    /// TODO consolidate this method into V2Handler, turn the parameters into fields and
    /// implement ConnTranslation::split()
    async fn v2_send_task<S>(
        mut conn_sender: S,
        mut translation_receiver: mpsc::Receiver<v2::Frame>,
        peer_addr: SocketAddr,
    ) -> Result<()>
    where
        S: v2::FramedSink,
    {
        loop {
            // We use select! so that more than just the translation receiver as a source can be
            // added
            select! {
                // Send out frames translated into V2
                v2_translated_frame = translation_receiver.next().fuse() => {
                    Self::v2_try_send_frame(&mut conn_sender, v2_translated_frame, &peer_addr)
                        .await?;
                },
            }
        }
    }

    async fn run(self) -> Result<()> {
        let mut v1_translation_rx = self.v1_translation_rx;
        let mut translation = self.translation;

        // TODO make connections 'optional' so that we can remove them from the instance and use
        //  the rest of the instance in as 'borrowed mutable reference'.
        let (mut v1_conn_tx, mut v1_conn_rx) = self.v1_conn.split();
        let (v2_conn_tx, mut v2_conn_rx) = self.v2_conn.split();

        // TODO factor out the frame pumping functionality and append the JoinHandle of this task
        //  to the select statement to detect any problems and to terminate the translation, too
        // V1 message send out loop
        let v1_send_task = async move {
            while let Some(frame) = v1_translation_rx.next().await {
                if let Err(err) = v1_conn_tx.send(frame).await {
                    warn!("V1 connection failed: {}", err);
                    break;
                }
            }
        };
        tokio::spawn(v1_send_task);

        tokio::spawn(Self::v2_send_task(
            v2_conn_tx,
            self.v2_translation_rx,
            self.v2_peer_addr,
        ));

        // TODO: add cancel handler into the select statement
        loop {
            select! {
                // Receive V1 frame and translate it to V2 message
                v1_frame = v1_conn_rx.next().timeout(Self::V1_UPSTREAM_TIMEOUT).fuse()=> {
                    // Unwrap the potentially elapsed timeout
                    match v1_frame? {
                        Some(v1_frame) => {
                            Self::v1_handle_frame(&mut translation, v1_frame?).await?;
                        }
                        None => {
                            Err(format!(
                                "Upstream V1 stratum connection dropped ({:?})",
                                self.v1_peer_addr
                            ))?;
                        }
                    }
                },
                // Receive V2 frame and translate it to V1 message
                v2_frame = v2_conn_rx.next().timeout(Self::V2_DOWNSTREAM_TIMEOUT).fuse() => {
                    match v2_frame? {
                        Some(v2_frame) => {
                            Self::v2_handle_frame(&mut translation, v2_frame?).await?;
                        }
                        None => {
                            Err(format!("V2 client disconnected ({:?})", self.v2_peer_addr))?;
                        }
                    }
                }
            }
        }
    }
}

pub async fn handle_connection<T: Send + Sync>(
    v2_conn: v2::Framed,
    v2_peer_addr: SocketAddr,
    v1_conn: v1::Framed,
    v1_peer_addr: SocketAddr,
    _generic_context: T,
) -> Result<()> {
    let translation = ConnTranslation::new(v2_conn, v2_peer_addr, v1_conn, v1_peer_addr);

    translation.run().await
}

/// Security context is held by the server and provided to each (noise secured) connection so
/// that it can successfully perform the noise handshake and authenticate itself to the client
/// NOTE: this struct doesn't intentionally derive Debug to prevent leakage of the secure key
/// into log messages
struct SecurityContext {
    /// Serialized Signature noise message that contains the necessary part of the certificate for
    /// succesfully authenticating with the Initiator. We store it as Bytes as it will be shared
    /// to among all incoming connections
    signature_noise_message: Bytes,
    /// Static key pair that the server will use within the noise handshake
    static_key_pair: v2::noise::StaticKeypair,
}

impl SecurityContext {
    fn from_certificate_and_secret_key(
        certificate: v2::noise::auth::Certificate,
        secret_key: v2::noise::auth::StaticSecretKeyFormat,
    ) -> Result<Self> {
        let signature_noise_message = certificate
            .build_noise_message()
            .serialize_to_bytes_mut()?
            .freeze();
        // TODO secret key validation is currently not possible
        //let public_key = certificate.validate_secret_key(&secret_key)?;
        let static_key_pair = v2::noise::StaticKeypair {
            private: secret_key.into_inner(),
            public: certificate.public_key.into_inner(),
        };

        Ok(Self {
            signature_noise_message,
            static_key_pair,
        })
    }
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Deserialize)]
pub enum ProxyProtocolVersion {
    V1,
    V2,
    Both,
}

impl std::str::FromStr for ProxyProtocolVersion {
    type Err = Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        use ProxyProtocolVersion::*;
        Ok(match s {
            "v1" => V1,
            "v2" => V2,
            "both" => Both,
            _ => return Err(Error::General("Invalid PROXY protocol version".into())),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProxyConfig {
    /// Accepts PROXY protocol header on downstream connection
    pub accept_proxy_protocol: bool,
    /// PROXY protocol is optional on incoming connection, if not present incoming connection works like normal TCP connection
    pub proxy_protocol_optional: bool,
    /// Accepted versions of PROXY protocol on incoming connection - either only V1 or V2 or both
    pub accepted_proxy_protocol_versions: ProxyProtocolVersion,
    /// If proxy protocol information is available from downstream connection,
    /// passes it to upstream connection
    pub pass_proxy_protocol: Option<ProxyProtocolVersion>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            accept_proxy_protocol: false,
            proxy_protocol_optional: false,
            accepted_proxy_protocol_versions: ProxyProtocolVersion::Both,
            pass_proxy_protocol: None,
        }
    }
}

/// Differentiate if we have TCPStream or ProxyStream
enum IncomingConnection {
    Tcp(TcpStream),
    Proxy(ProxyStream<TcpStream>),
}

impl WithProxyInfo for IncomingConnection {
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        use IncomingConnection::*;
        match self {
            Tcp(ref s) => s.original_peer_addr(),
            Proxy(ref p) => p.original_peer_addr(),
        }
    }

    fn original_destination_addr(&self) -> Option<SocketAddr> {
        use IncomingConnection::*;
        match self {
            Tcp(ref s) => s.original_destination_addr(),
            Proxy(ref p) => p.original_destination_addr(),
        }
    }
}

struct ProxyConnection<FN, T> {
    /// Downstream connection that is to be handled
    v2_downstream_conn: TcpStream,
    /// Upstream server that we should try to connect to
    v1_upstream_addr: Address,
    /// See ProxyServer
    get_connection_handler: Arc<FN>,
    /// Security context for noise handshake
    security_context: Option<Arc<SecurityContext>>,
    generic_context: T,
    /// Configuration of PROXY protocol
    proxy_config: ProxyConfig,
}

impl<FN, FT, T> ProxyConnection<FN, T>
where
    FT: Future<Output = Result<()>>,
    FN: Fn(v2::Framed, SocketAddr, v1::Framed, SocketAddr, T) -> FT,
    T: Send + Sync + Clone,
{
    fn new(
        v2_downstream_conn: TcpStream,
        v1_upstream_addr: Address,
        security_context: Option<Arc<SecurityContext>>,
        get_connection_handler: Arc<FN>,
        generic_context: T,
        proxy_config: ProxyConfig,
    ) -> Self {
        Self {
            v2_downstream_conn,
            v1_upstream_addr,
            get_connection_handler,
            security_context,
            generic_context,
            proxy_config,
        }
    }

    /// Handle incoming connection:
    ///  - establish upstream V1 connection
    ///  - check PROXY protocol header (if configured)
    ///  - pass PROXY protocol header (if configured)
    ///  - establish noise handshake (if configured)
    async fn do_handle(self, v2_peer_addr: SocketAddr) -> Result<()> {
        let incoming = if self.proxy_config.accept_proxy_protocol {
            use ProxyProtocolVersion::*;
            let (support_v1, support_v2) = match self.proxy_config.accepted_proxy_protocol_versions
            {
                V1 => (true, false),
                V2 => (false, true),
                Both => (true, true),
            };
            let acceptor = Acceptor::new()
                .support_v1(support_v1)
                .support_v2(support_v2)
                .require_proxy_header(!self.proxy_config.proxy_protocol_optional);
            let stream = acceptor.accept(self.v2_downstream_conn).await?;
            debug!("Received connection from downstream proxy - original source, {:?} original destination {:?}",
                   stream.original_peer_addr(), stream.original_destination_addr());
            IncomingConnection::Proxy(stream)
        } else {
            IncomingConnection::Tcp(self.v2_downstream_conn)
        };
        // Connect to upstream V1 server
        let mut v1_client = Client::new(self.v1_upstream_addr);
        // TODO Attempt only once to connect -> consider using the backoff for a few rounds before
        // failing. Also
        // Use the connection only to build the Framed object with V1 framing and to extract the
        // peer address
        let mut v1_conn = v1_client.next().await?;

        if let Some(version) = self.proxy_config.pass_proxy_protocol {
            if let (src @ Some(_), dst @ Some(_)) = (
                incoming.original_peer_addr(),
                incoming.original_destination_addr(),
            ) {
                Connector::new()
                    .use_v2(version == ProxyProtocolVersion::V2)
                    .connect_to(&mut v1_conn, src, dst)
                    .await?;
            } else {
                warn!("Passing of proxy protocol is required, but incoming connection does not contain original addresses")
            }
        }
        let v1_peer_addr = v1_conn.peer_addr()?;
        let v1_framed_stream = Connection::<v1::Framing>::new(v1_conn).into_inner();
        info!(
            "Established translation connection with upstream V1 {} for V2 peer: {}",
            v1_peer_addr, v2_peer_addr
        );

        let v2_framed_stream =
            match self.security_context {
                // Establish noise responder and run the handshake
                Some(security_context) => {
                    // TODO pass the signature message once the Responder API is adjusted
                    let responder = v2::noise::Responder::new(
                        &security_context.static_key_pair,
                        security_context.signature_noise_message.clone(),
                        vec![
                            v2::noise::negotiation::EncryptionAlgorithm::AESGCM,
                            v2::noise::negotiation::EncryptionAlgorithm::ChaChaPoly,
                        ],
                    );
                    let build_stratum_noise_codec = |noise_codec| {
                        <v2::Framing as ii_wire::Framing>::Codec::new(Some(noise_codec))
                    };
                    match incoming {
                        IncomingConnection::Proxy(proxy) => {
                            // TODO this needs refactoring there is no point of passing the codec
                            // type, we should be able to run noise just with anything that
                            // implements AsyncRead/AsyncWrite
                            responder
                            .accept_parts_with_codec(FramedParts::<TcpStream,
                                ii_wire::proxy::codec::v1::V1Codec>::from(proxy),
                                                     build_stratum_noise_codec)
                            .await?
                        }
                        IncomingConnection::Tcp(stream) => {
                            responder
                                .accept_with_codec(stream, build_stratum_noise_codec)
                                .await?
                        }
                    }
                }
                // Insecure operation has been configured
                None => match incoming {
                    IncomingConnection::Proxy(proxy) => {
                        Connection::<v2::Framing>::from(proxy).into_inner()
                    }
                    IncomingConnection::Tcp(stream) => {
                        Connection::<v2::Framing>::new(stream).into_inner()
                    }
                },
            };

        // Start processing of both ends
        // TODO adjust connection handler to return a Result
        (self.get_connection_handler)(
            v2_framed_stream,
            v2_peer_addr,
            v1_framed_stream,
            v1_peer_addr,
            self.generic_context.clone(),
        )
        .await
    }

    /// Handle connection by delegating it to a method that is able to handle a Result so that we
    /// have info/error reporting in a single place
    async fn handle(self) {
        let v2_peer_addr = match self.v2_downstream_conn.peer_addr() {
            Ok(a) => a,
            Err(err) => {
                debug!("Connection error: {}, can't retrieve peer address", err);
                return;
            }
        };

        match self.do_handle(v2_peer_addr).await {
            Ok(()) => info!("Closing connection from {:?} ...", v2_peer_addr),
            Err(err) => warn!("Connection error: {}, peer: {:?}", err, v2_peer_addr),
        }
    }
}

/// Structure representing the main server task.
///
/// Created by binding a listening socket.
/// Incoming connections are handled either by calling `next()` in a loop,
/// (a stream-like interface) or, as a higher-level interface,
/// the `run()` method turns the `ProxyServer`
/// into an asynchronous task (which internally calls `next()` in a loop).
pub struct ProxyServer<FN, T> {
    server: Server,
    listen_addr: Address,
    v1_upstream_addr: Address,
    quit_tx: mpsc::Sender<()>,
    quit_rx: Option<mpsc::Receiver<()>>,
    /// Closure that generates a handler in the form of a Future that will be passed to the
    get_connection_handler: Arc<FN>,
    /// Security context for noise handshake
    security_context: Option<Arc<SecurityContext>>,
    generic_context: T,
    /// PROXY protocol configuration
    proxy_config: ProxyConfig,
}

impl<FN, FT, T> ProxyServer<FN, T>
where
    FT: Future<Output = Result<()>> + Send + 'static,
    FN: Fn(v2::Framed, SocketAddr, v1::Framed, SocketAddr, T) -> FT + Send + Sync + 'static,
    T: Send + Sync + Clone + 'static,
{
    /// Constructor, binds the listening socket and builds the `ProxyServer` instance with a
    /// specified `get_connection_handler` that builds the connection handler `Future` on demand
    pub fn listen(
        listen_addr: Address,
        stratum_addr: Address,
        get_connection_handler: FN,
        certificate_secret_key_pair: Option<(
            v2::noise::auth::Certificate,
            v2::noise::auth::StaticSecretKeyFormat,
        )>,
        generic_context: T,
        proxy_config: ProxyConfig,
    ) -> Result<ProxyServer<FN, T>> {
        let server = Server::bind(&listen_addr)?;

        let (quit_tx, quit_rx) = mpsc::channel(1);

        let security_context = match certificate_secret_key_pair {
            Some((certificate, secret_key)) => Some(Arc::new(
                SecurityContext::from_certificate_and_secret_key(certificate, secret_key)?,
            )),
            None => None,
        };

        Ok(ProxyServer {
            server,
            listen_addr,
            v1_upstream_addr: stratum_addr,
            quit_rx: Some(quit_rx),
            quit_tx,
            get_connection_handler: Arc::new(get_connection_handler),
            security_context,
            generic_context,
            proxy_config,
        })
    }

    /// Obtain the quit channel transmit end,
    /// which can be used to terminate the server task.
    pub fn quit_channel(&self) -> mpsc::Sender<()> {
        self.quit_tx.clone()
    }

    /// Helper method for accepting incoming connections
    async fn accept(&self, connection_result: std::io::Result<TcpStream>) -> Result<SocketAddr> {
        let connection = connection_result?;

        let peer_addr = connection.peer_addr()?;

        // Fully secured connection has been established
        tokio::spawn(
            ProxyConnection::new(
                connection,
                self.v1_upstream_addr.clone(),
                self.security_context
                    .as_ref()
                    .map(|context| context.clone()),
                self.get_connection_handler.clone(),
                self.generic_context.clone(),
                self.proxy_config,
            )
            .handle(),
        );

        Ok(peer_addr)
    }

    /// Handle a connection. Call this in a loop to make the `ProxyServer`
    /// perform its job while being able to handle individual connection errors.
    ///
    /// This is a Stream-like interface but not actually implemented using a Stream
    /// because Stream doesn't get on very well with async.
    pub async fn next(&mut self) -> Option<Result<SocketAddr>> {
        // Select over the incoming connections stream and the quit channel
        // In case quit_rx is closed (by quit_tx being dropped),
        // we drop quit_rx as well and switch to only awaiting the socket.
        // Note that functional style can't really be used here because
        // unfortunately you can't await in map() et al.
        let conn = match self.quit_rx {
            Some(ref mut quit_rx) => {
                match future::select(self.server.next(), quit_rx.next()).await {
                    Either::Left((Some(conn), _)) => Some(conn),
                    Either::Right((None, _)) => {
                        // The quit_rx channel has been closed / quit_tx dropped,
                        // and so we can't poll the quit_rx any more (otherwise it panics)
                        self.quit_rx = None;
                        None
                    }
                    _ => return None, // Quit notification on quit_rx or socket closed
                }
            }
            None => None,
        };

        // If conn is None at this point, the quit_rx is no longer open
        // and we can just await the socket
        let conn = match conn {
            Some(conn) => conn,
            None => match self.server.next().await {
                Some(conn) => conn,
                None => return None, // Socket closed
            },
        };

        // Remap the connection stratum error into stratum proxy local error
        Some(self.accept(conn).await)
    }

    /// Creates a proxy server task that calls `.next()`
    /// in a loop with the default error handling.
    /// The default handling simply logs all
    /// connection errors via the logging crate.
    pub async fn run(mut self) {
        info!(
            "Stratum proxy service starting @ {} -> {}",
            self.listen_addr, self.v1_upstream_addr
        );

        while let Some(result) = self.next().await {
            match result {
                Ok(peer) => info!("Connection accepted from {}", peer),
                Err(err) => error!("Connection error: {}", err),
            }
        }

        info!("Stratum proxy service terminated");
    }
}
