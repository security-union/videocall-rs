/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use std::net::ToSocketAddrs;

use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use tracing::{error, info};

use sec_api::{
    diagnostics::health_processor,
    webtransport::{self, Certs},
};

async fn health_responder() -> impl Responder {
    HttpResponse::Ok().body("Ok")
}

#[get("/metrics")]
pub async fn metrics() -> impl Responder {
    health_processor::metrics_handler().await
}

#[actix_rt::main]
async fn main() {
    // Turned this off because it's too verbose
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    let health_listen = std::env::var("HEALTH_LISTEN_URL")
        .expect("expected HEALTH_LISTEN_URL to be set")
        .to_socket_addrs()
        .expect("expected HEALTH_LISTEN_URL to be a valid socket address")
        .next()
        .expect("expected HEALTH_LISTEN_URL to be a valid socket address");

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

    let listen = opt.listen;
    actix_rt::spawn(async move {
        info!("Starting http server: {:?}", listen);
        let server = HttpServer::new(|| {
            App::new()
                .route("/healthz", web::get().to(health_responder))
                .service(metrics)
        })
        .bind(&health_listen)
        .unwrap();
        if let Err(e) = server.run().await {
            error!("http server error: {}", e);
        }
    });

    let _ = actix_rt::spawn(async move {
        webtransport::start(opt).await.unwrap();
    })
    .await;
}
