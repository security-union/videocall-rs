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

use actix::Actor;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use tracing::{error, info};

use sec_api::actors::chat_server::ChatServer;
use sec_api::server_diagnostics::ServerDiagnostics;
use sec_api::session_manager::SessionManager;
use sec_api::webtransport::{self, Certs};

async fn health_responder() -> impl Responder {
    HttpResponse::Ok().body("Ok")
}

#[actix_rt::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    info!("Starting WebTransport server with actor-based session handling");

    // Connect to NATS
    let nats_url = std::env::var("NATS_URL").expect("NATS_URL env var must be defined");
    let nats_client = async_nats::ConnectOptions::new()
        .require_tls(false)
        .ping_interval(std::time::Duration::from_secs(10))
        .no_echo()
        .connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");
    info!("Connected to NATS at {}", nats_url);

    let chat_server = ChatServer::new(nats_client.clone()).await.start();
    info!("ChatServer actor started");

    // Create SessionManager
    let session_manager = SessionManager::new();

    // Create connection tracker with message channel
    let (connection_tracker, tracker_sender, tracker_receiver) =
        ServerDiagnostics::new_with_channel(nats_client.clone());

    // Start the connection tracker message processing task
    let connection_tracker = std::sync::Arc::new(connection_tracker);
    let tracker_task = connection_tracker.clone();
    tokio::spawn(async move {
        tracker_task.run_message_loop(tracker_receiver).await;
    });

    // Health server setup
    let health_listen = std::env::var("HEALTH_LISTEN_URL")
        .expect("expected HEALTH_LISTEN_URL to be set")
        .to_socket_addrs()
        .expect("expected HEALTH_LISTEN_URL to be a valid socket address")
        .next()
        .expect("expected HEALTH_LISTEN_URL to be a valid socket address");

    // WebTransport server options
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

    // Start health server
    actix_rt::spawn(async move {
        info!("Starting health/metrics HTTP server: {:?}", health_listen);
        let server =
            HttpServer::new(|| App::new().route("/healthz", web::get().to(health_responder)));

        match server.bind(&health_listen) {
            Ok(server) => {
                info!("Health server successfully bound to: {:?}", health_listen);
                if let Err(e) = server.run().await {
                    error!("Health server runtime error: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to bind health server to {:?}: {}", health_listen, e);
            }
        }
    });

    // Start WebTransport server with ChatServer
    let _ = actix_rt::spawn(async move {
        if let Err(e) = webtransport::start(
            opt,
            chat_server,
            nats_client,
            tracker_sender,
            session_manager,
        )
        .await
        {
            error!("WebTransport server error: {}", e);
        }
    })
    .await;
}
