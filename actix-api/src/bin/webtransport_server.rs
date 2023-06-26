use structopt::StructOpt;
use tracing::{error, info};

use sec_api::webtransport;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    let opt = webtransport::WebTransportOpt::from_args();
    match webtransport::start(opt).await {
        Ok(_) => info!("webtransport server stopped"),
        Err(e) => error!("webtransport server error: {}", e),
    }
}
