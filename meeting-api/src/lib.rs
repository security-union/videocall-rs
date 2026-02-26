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

//! Meeting Backend API library.
//!
//! This crate provides the Axum router, application state, and configuration
//! for the Meeting Backend service. The binary entry point (`main.rs`) is a
//! thin wrapper that calls into this library.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod oauth;
pub mod routes;
pub mod state;
pub mod token;
