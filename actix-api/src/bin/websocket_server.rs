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

use actix::Actor;
use actix_web::{web, App, HttpServer};
use sec_api::{
    actors::chat_server::ChatServer, lobby::ws_connect_authenticated, models::AppState,
    server_diagnostics::ServerDiagnostics, session_manager::SessionManager, version,
};
use tracing::info;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .init();
    info!("start");

    let nats_url = std::env::var("NATS_URL").expect("NATS_URL env var must be defined");
    let nats_client = async_nats::ConnectOptions::new()
        .require_tls(false)
        .ping_interval(std::time::Duration::from_secs(10))
        .connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat = ChatServer::new(nats_client.clone()).await.start();

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

    // Login, sessions and profiles belong to meeting-api. The relay only
    // accepts room access tokens on `/lobby?token=` and serves `/version`.
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(AppState {
                chat: chat.clone(),
                nats_client: nats_client.clone(),
                tracker_sender: tracker_sender.clone(),
                session_manager: session_manager.clone(),
            }))
            .service(ws_connect_authenticated)
            .route("/version", web::get().to(version::websocket_version))
    })
    .bind((
        "0.0.0.0",
        std::env::var("ACTIX_PORT")
            .unwrap_or_else(|_| String::from("8080"))
            .parse::<u16>()
            .unwrap(),
    ))?
    .run()
    .await
}
