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

//! Library surface for the synthetic-bot crate.
//!
//! The `bot` crate is compiled both as a binary (`src/main.rs`) and as a
//! library. The library target exists so integration tests under `tests/`
//! can reach into the crate's internals without duplicating code. We
//! intentionally keep the public surface narrow — these modules are
//! *not* part of a stable API. They may move or change without notice.

pub mod aq_controller;
pub mod audio_producer;
pub mod config;
pub mod costume_renderer;
pub mod diagnostics_reporter;
pub mod ekg_renderer;
pub mod health_reporter;
pub mod inbound_stats;
pub mod metrics_server;
pub mod netsim;
pub mod netsim_profiles;
pub mod token;
pub mod transport;
pub mod video_encoder;
pub mod video_producer;
pub mod websocket_client;
pub mod webtransport_client;
