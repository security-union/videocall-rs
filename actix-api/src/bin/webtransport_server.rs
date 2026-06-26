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
use sec_api::metrics::{metrics_responder, spawn_scheduler_lag_probe};
use sec_api::server_diagnostics::ServerDiagnostics;
use sec_api::session_manager::SessionManager;
use sec_api::version;
use sec_api::webtransport::{self, Certs};

async fn health_responder() -> impl Responder {
    HttpResponse::Ok().body("Ok")
}

// This relay runs on a SINGLE-THREADED runtime by necessity.
//
// `#[actix_rt::main]` builds a current-thread tokio runtime (actix-rt never
// reads `worker_threads` / the `TOKIO_WORKER_THREADS` env var). It must be
// single-threaded because the per-session `WtChatSession` actor and its
// WebTransport stream/datagram I/O are driven on the actix `LocalSet` via
// `spawn_local` (see `webtransport::mod` connection-accept loop), which a
// multi-threaded runtime cannot host. Consequently `TOKIO_WORKER_THREADS` is
// INERT for this binary — setting it in deploy config does nothing, and
// thread count cannot relieve outbound back-pressure here.
//
// Scaling past one core (multi-Arbiter sharding / off-thread parse — issue
// #1639 options a/b) is future work, gated on the #1637 scheduler-lag
// instrumentation. Issue #1639 option (c) only removed the inert env and
// documented this ceiling.
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
        .connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");
    info!("Connected to NATS at {}", nats_url);

    // Start ChatServer actor
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

    // #1637 (epic #1636, INSURANCE SIGNAL): tokio scheduler-lag probe.
    //
    // This relay runs on a SINGLE-THREADED `#[actix_rt::main]` runtime, so a long
    // synchronous span or fan-out burst on that one thread stalls EVERY task at
    // once — the latent Gun #2 (#1639). The cgroup CPU average cannot resolve such
    // a sub-second stall (a 200ms freeze vanishes in a multi-second average). The
    // only way to see it is to measure how late a timer that SHOULD fire on THIS
    // runtime actually fires.
    //
    // The probe spawn + loop live in `metrics::spawn_scheduler_lag_probe` /
    // `run_scheduler_lag_probe` (library fns, so the `actix_rt::spawn` + loop +
    // `.observe()` wiring is unit-testable — an inline spawn here would not be,
    // leaving it unguarded; mirrors `webtransport::spawn_connection_path_sampler`).
    // It spawns on `actix_rt` ON PURPOSE: the probe must live on the SAME
    // single-thread runtime whose lag we want to measure — a probe on any other
    // thread would measure that thread's scheduler, not the relay's. See
    // `run_scheduler_lag_probe` for the deadline-based measurement (correct under
    // both MissedTickBehavior variants) and the histogram-vs-gauge rationale. This
    // `main() -> spawn_scheduler_lag_probe()` line is the irreducible composition
    // boundary (like the NATS-connect / health-bind / webtransport::start calls
    // around it — `main` is not unit-tested).
    const SCHEDULER_LAG_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    spawn_scheduler_lag_probe(SCHEDULER_LAG_PROBE_INTERVAL);

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
        let server = HttpServer::new(|| {
            App::new()
                .route("/healthz", web::get().to(health_responder))
                .route("/metrics", web::get().to(metrics_responder))
                .route("/version", web::get().to(version::webtransport_version))
        });

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
