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

//! # Shared NetEq Audio Architecture
//!
//! When the `neteq_ff` feature is enabled, the system uses a shared AudioContext
//! with individual PCM worklets per peer:
//!
//! ```
//! SharedNetEqAudioManager
//! ├── AudioContext (shared)
//! ├── Peer "alice" → NetEq Worker + PCM Worklet
//! ├── Peer "bob"   → NetEq Worker + PCM Worklet  
//! └── Device Management (affects all peers)
//! ```

pub mod neteq_peer_sink;
pub mod shared_neteq_audio_manager;

pub use neteq_peer_sink::NetEqPeerSink;
pub use shared_neteq_audio_manager::SharedNetEqAudioManager;
