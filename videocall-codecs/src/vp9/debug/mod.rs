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

//! Minimal VP9 stream parsers used purely for our own round-trip validation
//! (they mirror the decoder field order, not a full decoder). Gated behind
//! `test`/`test-utils`.

#![allow(dead_code)] // consumed in later milestones

pub mod parser;
