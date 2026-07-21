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

//! Shared test helpers for meeting-api integration tests.
//!
//! Every test in this directory runs against whichever backend the crate was
//! compiled with, so the whole suite is executed twice in CI:
//!
//! ```text
//! cargo test -p meeting-api                                    # postgres
//! cargo test -p meeting-api --no-default-features --features sqlite
//! ```
//!
//! Tests only ever see [`meeting_api::db::DbPool`], and statements written here
//! go through [`meeting_api::db::q`] exactly like the ones in `src/db`, so a
//! test file never needs a `cfg` of its own.

#![allow(dead_code)]

use axum::http;
use axum::response::Response;
use axum::Router;
use http_body_util::BodyExt;
use meeting_api::db::{q, DbPool};
use meeting_api::{routes, state::AppState, token::generate_session_token};
use serde::de::DeserializeOwned;

pub const TEST_JWT_SECRET: &str = "test-secret-for-integration-tests";
const TEST_TOKEN_TTL: i64 = 600;
const TEST_SESSION_TTL: i64 = 3600;

/// Connect to the test database.
///
/// PostgreSQL uses `DATABASE_URL`, pointing at a database dbmate has already
/// migrated. SQLite creates a throwaway database file and migrates it from
/// `dbmate/sqlite/db/migrations` on first use; see [`sqlite_support`].
///
/// Both paths go through [`meeting_api::db::connect`] — the same constructor
/// `main` uses — so per-connection settings (`foreign_keys`, `busy_timeout`,
/// WAL) are exercised rather than reimplemented here.
pub async fn get_test_pool() -> DbPool {
    #[cfg(feature = "postgres")]
    {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        meeting_api::db::connect(&url).await
    }
    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    {
        sqlite_support::migrated_pool().await
    }
}

/// Delete all test data for a given `room_id` (participants first due to FK).
pub async fn cleanup_test_data(pool: &DbPool, room_id: &str) {
    let _ = sqlx::query(&q("DELETE FROM meeting_participants WHERE meeting_id IN \
         (SELECT id FROM meetings WHERE room_id = $1)"))
    .bind(room_id)
    .execute(pool)
    .await;

    let _ = sqlx::query(&q("DELETE FROM meetings WHERE room_id = $1"))
        .bind(room_id)
        .execute(pool)
        .await;
}

/// Build the Axum router backed by the given pool, ready for `tower::ServiceExt::oneshot`.
pub fn build_app(pool: DbPool) -> Router {
    let state = AppState {
        db: pool,
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: TEST_TOKEN_TTL,
        session_ttl_secs: TEST_SESSION_TTL,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: None,
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
    };
    routes::router().with_state(state)
}

/// Build an HTTP request with a signed session JWT in the `Cookie: session=<jwt>` header.
///
/// This replaces the old `Cookie: email=<email>` pattern. The JWT is signed
/// with [`TEST_JWT_SECRET`] and contains the email in the `sub` claim.
pub fn request_with_cookie(method: &str, uri: &str, email: &str) -> http::request::Builder {
    let session_jwt = generate_session_token(TEST_JWT_SECRET, email, email, TEST_SESSION_TTL)
        .expect("signing session JWT for test should not fail");
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Cookie", format!("session={session_jwt}"))
}

/// Consume a response body and deserialize JSON into `T`.
pub async fn response_json<T: DeserializeOwned>(resp: Response) -> T {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("deserialize response body")
}

/// Test-database plumbing that only exists for the SQLite build.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub mod sqlite_support {
    use meeting_api::db::DbPool;
    use std::path::PathBuf;
    use tokio::sync::OnceCell;

    /// One migrated database per test binary.
    ///
    /// Cargo runs each integration test file as its own process, so this gives
    /// every file an isolated database while the `#[serial]` attribute the
    /// suite already uses keeps tests inside a file from overlapping. A file —
    /// not `:memory:` — because WAL needs a real file and because an in-memory
    /// SQLite database is private to a single connection, which a pool of five
    /// would silently turn into five empty databases.
    static POOL: OnceCell<DbPool> = OnceCell::const_new();

    /// A pool over a freshly migrated throwaway database, shared per binary.
    pub async fn migrated_pool() -> DbPool {
        POOL.get_or_init(|| async {
            let path = scratch_db_path();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("db-wal"));
            let _ = std::fs::remove_file(path.with_extension("db-shm"));

            let pool = new_pool(&path).await;
            migrate(&pool).await;
            pool
        })
        .await
        .clone()
    }

    /// Open a pool on `path` through the application's own constructor.
    ///
    /// Tests must not hand-roll a `SqliteConnectOptions`: the pragmas that
    /// constructor sets (`foreign_keys` above all) are exactly what the schema
    /// tests are checking, and a private connection would test the test.
    pub async fn new_pool(path: &std::path::Path) -> DbPool {
        meeting_api::db::connect(&format!("sqlite://{}", path.display())).await
    }

    /// A unique, writable database path for this test process.
    pub fn scratch_db_path() -> PathBuf {
        let dir = std::env::temp_dir().join("meeting-api-tests");
        std::fs::create_dir_all(&dir).expect("create test scratch dir");
        dir.join(format!("test-{}.db", std::process::id()))
    }

    /// Apply the real dbmate migrations from `dbmate/sqlite/db/migrations`.
    ///
    /// Deliberately reads the shipped `.sql` files rather than embedding a
    /// second copy of the schema: the point of the schema tests is to check
    /// what production actually runs. The `-- migrate:up` section is applied in
    /// filename order, which is dbmate's ordering.
    pub async fn migrate(pool: &DbPool) {
        for sql in up_migrations() {
            sqlx::raw_sql(&sql)
                .execute(pool)
                .await
                .expect("apply SQLite migration");
        }
    }

    /// The `-- migrate:up` body of every migration, in filename order.
    pub fn up_migrations() -> Vec<String> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../dbmate/sqlite/db/migrations")
            .canonicalize()
            .expect("dbmate/sqlite/db/migrations must exist");

        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read migrations dir")
            .map(|e| e.expect("read migration entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no SQLite migrations found in {dir:?}");

        files
            .iter()
            .map(|path| {
                let body = std::fs::read_to_string(path).expect("read migration");
                let up = body
                    .split_once("-- migrate:up")
                    .unwrap_or_else(|| panic!("{path:?} has no `-- migrate:up`"))
                    .1;
                up.split_once("-- migrate:down")
                    .map(|(up, _)| up)
                    .unwrap_or(up)
                    .to_string()
            })
            .collect()
    }
}
