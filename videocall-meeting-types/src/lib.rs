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

//! Shared API types for the videocall.rs meeting backend.
//!
//! This crate defines the API contract between the Meeting Backend
//! and its consumers (clients, frontend, integration tests).
//! It is intentionally framework-agnostic — no actix-web, no database types.

pub mod error;
pub mod requests;
pub mod responses;
pub mod token;

pub use error::APIError;
pub use responses::APIResponse;
pub use token::RoomAccessTokenClaims;
