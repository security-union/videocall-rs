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

//! Async database pool using sqlx
//!
//! This is the preferred way to access the database.
//! Use this instead of the sync r2d2 pool in db/mod.rs

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::env;
use std::time::Duration;
use tracing::info;

/// Get the database URL from environment
pub fn get_database_url() -> Option<String> {
    env::var("DATABASE_URL").ok()
}

/// Create an async PostgreSQL connection pool
pub async fn create_pool() -> Result<PgPool, sqlx::Error> {
    let database_url = get_database_url().expect("DATABASE_URL must be set");

    info!("Connecting to database...");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    info!("Database connection pool established");
    Ok(pool)
}

/// Try to create a pool, returns None if DATABASE_URL is not set
pub async fn try_create_pool() -> Option<PgPool> {
    match get_database_url() {
        Some(url) => {
            info!("Connecting to database...");
            match PgPoolOptions::new()
                .max_connections(10)
                .acquire_timeout(Duration::from_secs(5))
                .connect(&url)
                .await
            {
                Ok(pool) => {
                    info!("Database connection pool established");
                    Some(pool)
                }
                Err(e) => {
                    tracing::error!("Failed to connect to database: {}", e);
                    None
                }
            }
        }
        None => {
            tracing::warn!("DATABASE_URL not set, running without database");
            None
        }
    }
}
