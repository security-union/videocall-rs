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
 */

//! Meeting Backend API server entry point.
//!
//! A standalone Axum service that manages meetings, waiting rooms,
//! and issues JWT room access tokens for the Media Server.

use axum::http;
use meeting_api::config::Config;
use meeting_api::routes;
use meeting_api::state::AppState;
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env().expect("failed to load configuration");

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .expect("failed to connect to PostgreSQL");

    tracing::info!("Connected to PostgreSQL");

    // CORS: In production set `CORS_ALLOWED_ORIGIN` to the exact frontend
    // origin (e.g. "https://app.videocall.rs").  When unset, the server
    // mirrors the request origin which is convenient for development but
    // **insecure** in production (any site can make credentialed requests).
    //
    // `allow_credentials(true)` requires explicit methods and headers (not *).
    let cors = CorsLayer::new()
        .allow_origin(match &config.cors_allowed_origin {
            Some(origin) => {
                let hv: http::HeaderValue = origin.parse().expect("invalid CORS_ALLOWED_ORIGIN");
                AllowOrigin::exact(hv)
            }
            None => AllowOrigin::mirror_request(),
        })
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::PUT,
            http::Method::DELETE,
            http::Method::OPTIONS,
        ])
        .allow_headers([
            http::header::CONTENT_TYPE,
            http::header::AUTHORIZATION,
            http::header::COOKIE,
            http::header::ACCEPT,
        ])
        .allow_credentials(true);

    let state = AppState::new(pool, &config);
    let app = routes::router().layer(cors).with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .expect("failed to bind listener");

    tracing::info!("Meeting Backend listening on {}", config.listen_addr);

    axum::serve(listener, app).await.expect("server error");
}
