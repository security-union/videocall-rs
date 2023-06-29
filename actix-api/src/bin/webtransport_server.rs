use std::net::ToSocketAddrs;

use tracing::{error, info};

use sec_api::webtransport::{self, Certs};

#[tokio::main]
async fn main() {
    // Turned this off because it's too verbose
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    let opt = webtransport::WebTransportOpt {
        listen: std::env::var("LISTEN_URL")
            .expect("expected LISTEN_URL to be set")
            .to_socket_addrs()
            .expect("expected LISTEN_URL to be a valid socket address")
            .next()
            .expect("expected LISTEN_URL to be a valid socket address"),
        certs: Certs {
            key: std::env::var("KEY_PATH")
                .expect("expected KEY_PATH to be set")
                .into(),
            cert: std::env::var("CERT_PATH")
                .expect("expected CERT_PATH to be set")
                .into(),
        },
    };
    match webtransport::start(opt).await {
        Ok(_) => info!("webtransport server stopped"),
        Err(e) => error!("webtransport server error: {}", e),
    }
}
