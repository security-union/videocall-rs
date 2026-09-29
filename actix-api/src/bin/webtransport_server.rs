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
use actix_web::{web, App, HttpServer};
use tracing::{error, info};

use sec_api::actors::chat_server::ChatServer;
use sec_api::metrics::{metrics_responder, spawn_scheduler_lag_probe_on_slot};
use sec_api::relay_health;
use sec_api::relay_shards::SessionShards;
use sec_api::server_diagnostics::ServerDiagnostics;
use sec_api::session_manager::SessionManager;
use sec_api::version;
use sec_api::webtransport::{self, Certs};

// #2727: `WT_SESSION_ARBITERS` shards the relay. Past one it builds a
// MULTI-THREADED tokio runtime, for which `TOKIO_WORKER_THREADS` is LIVE and a
// bad value FATAL — so the pool size is passed explicitly below, which is what
// makes tokio ignore it. Leave it unset; use `WT_SESSION_ARBITERS`.
fn main() {
    // Before reading the arbiter count: resolving it warns on a rejected value,
    // and a `warn!` predating the subscriber is dropped by the facade.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();

    let arbiters = sec_api::relay_shards::session_arbiter_count();
    let workers = sec_api::relay_shards::resolve_worker_thread_count(arbiters);
    let runner = if sec_api::relay_shards::needs_multi_thread_runtime(arbiters) {
        actix_rt::System::with_tokio_rt(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .expect("failed to build the relay's multi-threaded tokio runtime")
        })
    } else {
        actix_rt::System::new()
    };
    runner.block_on(run(arbiters, workers));
}

async fn run(arbiters: usize, workers: usize) {
    info!("Starting WebTransport server with actor-based session handling");
    sec_api::startup::log_feature_flags();
    sec_api::metrics::init_webtransport_relay_series();

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

    // #1637 scheduler-lag probe, also the #2719 `/healthz` heartbeat. The main
    // runtime claims slot 0 and each arbiter its own, so `/healthz` reads the
    // OLDEST stamp.
    const SCHEDULER_LAG_PROBE_INTERVAL: std::time::Duration = relay_health::HEARTBEAT_PERIOD;
    let main_slot = relay_health::register_heartbeat_slot();
    spawn_scheduler_lag_probe_on_slot(SCHEDULER_LAG_PROBE_INTERVAL, main_slot);

    // Before the QUIC listener accepts, so no connection lands on a shard whose
    // heartbeat slot is unregistered.
    let mut shards = SessionShards::new(arbiters);
    let arbiter_probes = shards.spawn_scheduler_lag_probes(SCHEDULER_LAG_PROBE_INTERVAL);
    let shards = std::sync::Arc::new(shards);
    info!(
        "Session sharding: {} arbiter(s) ({} probed), {} tokio worker(s), override with {}",
        shards.len(),
        arbiter_probes,
        workers,
        sec_api::relay_shards::SESSION_ARBITERS_ENV
    );
    info!(
        "Relay liveness heartbeat: period={:?}, /healthz fails past {}ms (override with {})",
        SCHEDULER_LAG_PROBE_INTERVAL,
        relay_health::stale_threshold_ms(),
        relay_health::STALE_THRESHOLD_ENV
    );

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
        // actix-server's default is one worker thread per available core.
        let server = HttpServer::new(|| {
            App::new()
                .route(
                    "/healthz",
                    web::get().to(relay_health::relay_health_responder),
                )
                .route("/metrics", web::get().to(metrics_responder))
                .route("/version", web::get().to(version::webtransport_version))
        })
        .workers(1);

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
            shards,
        )
        .await
        {
            error!("WebTransport server error: {}", e);
        }
    })
    .await;
}
