use anyhow::{anyhow, Result};
use bytes::Bytes;
use http::Method;

use rustls::{Certificate, PrivateKey};
use sec_http3::error::Code;
use sec_http3::sec_http3_quinn as h3_quinn;
use sec_http3::webtransport::{server::WebTransportSession, stream};
use sec_http3::{
    error::ErrorLevel,
    ext::Protocol,
    quic::{self, RecvDatagramExt, SendDatagramExt, SendStreamUnframed},
    server::Connection,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use structopt::StructOpt;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{error, info, trace_span};

#[derive(StructOpt, Debug)]
#[structopt(name = "server")]
pub struct WebTransportOpt {
    #[structopt(
        short,
        long,
        default_value = "127.0.0.1:4433",
        help = "What address:port to listen for new connections"
    )]
    pub listen: SocketAddr,

    #[structopt(flatten)]
    pub certs: Certs,
}

#[derive(StructOpt, Debug)]
pub struct Certs {
    #[structopt(
        long,
        short,
        default_value = "examples/server.cert",
        help = "Certificate for TLS. If present, `--key` is mandatory."
    )]
    pub cert: PathBuf,

    #[structopt(
        long,
        short,
        default_value = "examples/server.key",
        help = "Private key for the certificate."
    )]
    pub key: PathBuf,
}

pub async fn start(opt: WebTransportOpt) -> Result<(), Box<dyn std::error::Error>> {
    info!("WebTransportOpt: {opt:#?}");
    let Certs { cert, key } = opt.certs;

    // both cert and key must be DER-encoded
    let cert = Certificate(std::fs::read(cert)?);
    let key = PrivateKey(std::fs::read(key)?);

    let mut tls_config = rustls::ServerConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;

    tls_config.max_early_data_size = u32::MAX;
    let alpn: Vec<Vec<u8>> = vec![
        b"h3".to_vec(),
        b"h3-32".to_vec(),
        b"h3-31".to_vec(),
        b"h3-30".to_vec(),
        b"h3-29".to_vec(),
    ];
    tls_config.alpn_protocols = alpn;

    // 1. create quinn server endpoint and bind UDP socket
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
    server_config.transport = Arc::new(transport_config);
    let endpoint = quinn::Endpoint::server(server_config, opt.listen)?;

    info!("listening on {}", opt.listen);

    // 2. Accept new quic connections and spawn a new task to handle them
    while let Some(new_conn) = endpoint.accept().await {
        trace_span!("New connection being attempted");

        tokio::spawn(async move {
            match new_conn.await {
                Ok(conn) => {
                    info!("new http3 established");
                    let h3_conn = sec_http3::server::builder()
                        .enable_webtransport(true)
                        .enable_connect(true)
                        .enable_datagram(true)
                        .max_webtransport_sessions(1)
                        .send_grease(true)
                        .build(h3_quinn::Connection::new(conn))
                        .await
                        .unwrap();

                    // info!("Establishing WebTransport session");
                    // // 3. TODO: Conditionally, if the client indicated that this is a webtransport session, we should accept it here, else use regular h3.
                    // // if this is a webtransport session, then h3 needs to stop handing the datagrams, bidirectional streams, and unidirectional streams and give them
                    // // to the webtransport session.

                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(h3_conn).await {
                            error!("Failed to handle connection: {err:?}");
                        }
                    });
                    // let mut session: WebTransportSession<_, Bytes> =
                    //     WebTransportSession::accept(h3_conn).await.unwrap();
                    // info!("Finished establishing webtransport session");
                    // // 4. Get datagrams, bidirectional streams, and unidirectional streams and wait for client requests here.
                    // // h3_conn needs to handover the datagrams, bidirectional streams, and unidirectional streams to the webtransport session.
                    // let result = handle.await;
                }
                Err(err) => {
                    error!("accepting connection failed: {:?}", err);
                }
            }
        });
    }

    // shut down gracefully
    // wait for connections to be closed before exiting
    endpoint.wait_idle().await;

    Ok(())
}

async fn handle_connection(mut conn: Connection<h3_quinn::Connection, Bytes>) -> Result<()> {
    // 3. TODO: Conditionally, if the client indicated that this is a webtransport session, we should accept it here, else use regular h3.
    // if this is a webtransport session, then h3 needs to stop handing the datagrams, bidirectional streams, and unidirectional streams and give them
    // to the webtransport session.

    loop {
        match conn.accept().await {
            Ok(Some((req, stream))) => {
                info!("new request: {:#?}", req);

                let ext = req.extensions();
                match req.method() {
                    &Method::CONNECT if ext.get::<Protocol>() == Some(&Protocol::WEB_TRANSPORT) => {
                        let uri = req.uri().clone();

                        info!("Got path : {} ", uri.path());

                        let parts = uri.path().split('/').collect::<Vec<&str>>();
                        // filter out the empty strings
                        let parts = parts.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
                        info!("Parts {:?}", parts);
                        if parts.len() != 3 {
                            conn.close(Code::H3_REQUEST_REJECTED, "Invalid path wrong length");
                            return Err(anyhow!("Invalid path wrong length"));
                        } else if parts[0] != &"lobby" {
                            conn.close(Code::H3_REQUEST_REJECTED, "Invalid path wrong prefix");
                            return Err(anyhow!("Invalid path wrong prefix"));
                        }

                        let username = parts[1];
                        let lobby_id = parts[2];

                        info!("Peer wants to initiate a webtransport session");

                        info!("Handing over connection to WebTransport");

                        let session = WebTransportSession::accept(req, stream, conn).await?;
                        info!("Established webtransport session");
                        // 4. Get datagrams, bidirectional streams, and unidirectional streams and wait for client requests here.
                        // h3_conn needs to handover the datagrams, bidirectional streams, and unidirectional streams to the webtransport session.
                        handle_session(session, username, lobby_id).await?;

                        return Ok(());
                    }
                    _ => {
                        info!(?req, "Received request");
                    }
                }
            }

            // indicating no more streams to be received
            Ok(None) => {
                break;
            }

            Err(err) => {
                error!("Error on accept {}", err);
                match err.get_error_level() {
                    ErrorLevel::ConnectionError => break,
                    ErrorLevel::StreamError => continue,
                }
            }
        }
    }
    Ok(())
}

#[tracing::instrument(level = "info", skip(session))]
async fn handle_session<C>(
    session: WebTransportSession<C, Bytes>,
    username: &str,
    lobby_id: &str,
) -> anyhow::Result<()>
where
    // Use trait bounds to ensure we only happen to use implementation that are only for the quinn
    // backend.
    C: 'static
        + Send
        + sec_http3::quic::Connection<Bytes>
        + RecvDatagramExt<Buf = Bytes>
        + SendDatagramExt<Bytes>,
    <C::SendStream as sec_http3::quic::SendStream<Bytes>>::Error:
        'static + std::error::Error + Send + Sync + Into<std::io::Error>,
    <C::RecvStream as sec_http3::quic::RecvStream>::Error:
        'static + std::error::Error + Send + Sync + Into<std::io::Error>,
    stream::BidiStream<C::BidiStream, Bytes>:
        quic::BidiStream<Bytes> + Unpin + AsyncWrite + AsyncRead,
    <stream::BidiStream<C::BidiStream, Bytes> as quic::BidiStream<Bytes>>::SendStream:
        Unpin + AsyncWrite + Send + Sync,
    <stream::BidiStream<C::BidiStream, Bytes> as quic::BidiStream<Bytes>>::RecvStream:
        Unpin + AsyncRead + Send + Sync,
    C::SendStream: Send + Unpin,
    C::RecvStream: Send + Unpin,
    C::BidiStream: Send + Unpin,
    stream::SendStream<C::SendStream, Bytes>: AsyncWrite,
    C::BidiStream: SendStreamUnframed<Bytes>,
    C::SendStream: SendStreamUnframed<Bytes>,
{
    let session_id = session.session_id();

    let nc =
        nats::asynk::connect(std::env::var("NATS_URL").expect("NATS_URL env var must be defined"))
            .await
            .unwrap();
    info!("Connected to NATS");

    let subject = format!("room.{}.*", lobby_id);
    let specific_subject = format!("room.{}.{}", lobby_id, username);
    let queue = format!("{:?}-{}", session_id, lobby_id);
    let sub = match nc.queue_subscribe(&subject, &queue).await {
        Ok(sub) => {
            info!("Subscribed to subject {}", subject);
            sub
        }
        Err(e) => {
            let err = format!("error subscribing to subject {}: {}", subject, e);
            error!("{}", err);
            return Err(anyhow!(err));
        }
    };

    loop {
        tokio::select! {
            datagram = session.accept_datagram() => {
                let datagram = datagram?;
                if let Some((_, datagram)) = datagram {
                    info!("Got datagram: {:?}", datagram);
                    nc.publish(&specific_subject, datagram).await.unwrap();
                }
            }
            _uni_stream = session.accept_uni() => {
                // TODO: Handle uni streams
            }
            _stream = session.accept_bi() => {
                // TODO: Handle bi streams
            }
            msg = sub.next() => {
                if let Some(msg) = msg {
                    if msg.subject == specific_subject {
                        continue;
                    }
                    session.send_datagram(msg.data.into()).unwrap();
                }
            }
            else => {
                break
            }
        }
    }

    info!("Finished handling session");

    Ok(())
}
