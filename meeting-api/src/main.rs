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
use axum::http::HeaderName;
use meeting_api::config::Config;
use meeting_api::cors::{ALLOWED_CUSTOM_HEADERS, ALLOWED_HEADERS, ALLOWED_METHODS};
use meeting_api::nats_consumers;
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

    let mut config = Config::from_env().expect("failed to load configuration");

    // Run OIDC discovery to fill in auth/token/jwks URLs when an issuer is configured.
    config
        .resolve_discovery()
        .await
        .expect("OIDC discovery failed");
    config
        .validate_oauth_security()
        .expect("invalid OAuth/JWKS security configuration");

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .expect("failed to connect to PostgreSQL");

    tracing::info!("Connected to PostgreSQL");

    // Connect to NATS if configured. The server works without NATS (graceful degradation).
    let nats = match &config.nats_url {
        Some(url) => match async_nats::connect(url).await {
            Ok(client) => {
                tracing::info!("Connected to NATS at {url}");
                Some(client)
            }
            Err(e) => {
                tracing::warn!("Failed to connect to NATS at {url}: {e}. Continuing without NATS.");
                None
            }
        },
        None => {
            tracing::warn!(
                "NATS_URL not set — meeting event push notifications disabled \
                 (mute, rename, waiting-room admit/reject, host-leave broadcast)"
            );
            None
        }
    };

    // CORS: In production set `CORS_ALLOWED_ORIGIN` to the exact frontend
    // origin (e.g. "https://app.videocall.rs").  Comma-separate for multiple
    // origins. When unset, the server mirrors the request origin which is
    // convenient for development but **insecure** in production (any site can
    // make credentialed requests).
    //
    // `AllowOrigin::list` echoes back only the matched origin so the response
    // header always contains a single value, which is required by the spec.
    //
    // `allow_credentials(true)` requires explicit methods and headers (not *).
    let cors = CorsLayer::new()
        .allow_origin(match config.cors_allowed_origin.as_slice() {
            [] => AllowOrigin::mirror_request(),
            origins => {
                let hvs: Vec<http::HeaderValue> = origins
                    .iter()
                    .map(|o| o.parse().expect("invalid CORS_ALLOWED_ORIGIN"))
                    .collect();
                AllowOrigin::list(hvs)
            }
        })
        .allow_methods(ALLOWED_METHODS.to_vec())
        .allow_headers(
            ALLOWED_HEADERS
                .iter()
                .cloned()
                .chain(
                    ALLOWED_CUSTOM_HEADERS
                        .iter()
                        .map(|h| HeaderName::from_static(h)),
                )
                .collect::<Vec<_>>(),
        )
        .allow_credentials(true);

    // Warn loudly at startup if DEV_USER auto-login is active.
    if let Some(ref dev_user) = config.dev_user {
        tracing::warn!(
            "DEV_USER is set — auto-login enabled for {} ({}). \
             DO NOT use in production.",
            dev_user.name,
            dev_user.email
        );
    }

    // Live homepage-feed change stream (issue #1081). `feed_tx` is this
    // process's broadcast that the SSE handler subscribes to. It is created
    // BEFORE the NATS consumers so they can push feed-change nudges into it: the
    // three room-state consumers below run on EVERY instance (fan-out, no queue
    // group), so each instance feeds its OWN local broadcast after the DB write
    // — nudging its own SSE clients exactly once without an echo loop. See
    // `meeting_api::feed_events` for the full multi-instance rationale.
    let (feed_tx, _feed_rx) = meeting_api::feed_events::new_feed_channel();

    // Spawn the cross-service NATS consumers BEFORE constructing the AppState
    // (so AppState retains its own clone of `nats`). Each consumer is a
    // long-lived task that re-subscribes on disconnect; we hold the JoinHandle
    // implicitly by leaking it (the task survives until process exit).
    let _ended_consumer = nats_consumers::spawn_meeting_ended_by_host_consumer(
        nats.clone(),
        pool.clone(),
        feed_tx.clone(),
    );
    let _empty_consumer = nats_consumers::spawn_meeting_became_empty_consumer(
        nats.clone(),
        pool.clone(),
        feed_tx.clone(),
    );
    // Marks a participant `status='left', left_at=NOW()` when actix-api reports
    // their session left a room (issue #1551), so an abnormal disconnect (no
    // REST /leave) stops being counted as a present participant.
    let _participant_left_consumer = nats_consumers::spawn_participant_left_consumer(
        nats.clone(),
        pool.clone(),
        feed_tx.clone(),
    );
    // Symmetric counterpart (issue #1628): when actix-api reports a participant
    // became PRESENT (a fresh join or a transport reconnect), restore their
    // `meeting_participants` row to `admitted` and re-activate the meeting
    // (`idle -> active`). Without this, a transport-only reconnect — which never
    // re-hits REST /join — left the meeting stuck `idle` with people in it.
    let _participant_present_consumer = nats_consumers::spawn_participant_present_consumer(
        nats.clone(),
        pool.clone(),
        feed_tx.clone(),
    );

    // Fan-out subscriber for the local-HTTP-mutation feed changes (create /
    // admit / join-reactivation / end / leave). Those mutation points run on
    // only ONE instance, so they publish a `FeedChange` to the dedicated NATS
    // subject `internal.feed_changed`; this subscriber (NO queue group → every
    // instance receives it) mirrors that subject into `feed_tx` so SSE clients
    // on ALL instances observe the change. Returns `None` (no-op) when NATS is
    // not configured; in that single-instance mode the mutation points feed
    // `feed_tx` directly. See `meeting_api::feed_events`.
    let _feed_change_consumer =
        meeting_api::feed_events::spawn_feed_change_consumer(nats.clone(), feed_tx.clone(), None);

    // Spawn the in-process console-log retention task. Returns `None` (no-op)
    // when `CONSOLE_LOG_UPLOAD_ENABLED` is not `"true"`. The handle is leaked
    // for the life of the process, mirroring the NATS consumer above.
    let _purge_handle = meeting_api::console_log_purge::spawn_purge_task();

    let state = AppState::new(pool, &config, nats, feed_tx);
    let app = routes::router().layer(cors).with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .expect("failed to bind listener");

    tracing::info!("Meeting Backend listening on {}", config.listen_addr);

    axum::serve(listener, app).await.expect("server error");
}
