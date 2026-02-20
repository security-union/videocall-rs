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

pub mod cli_args;
pub mod consumers;

pub mod producers;

/// Re-export the shared VP9 encoder from `videocall-codecs`.
///
/// Previously this was a local copy; now it lives in the `videocall-codecs` crate
/// as the single source of truth for VP9 encoding.
pub mod video_encoder {
    pub use videocall_codecs::encoder::*;
}
