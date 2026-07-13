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

//! Decoder-mandated, bit-exact VP9 machinery ported faithfully from libvpx
//! `vp9/common/` and `vpx_dsp/`. Errors here silently corrupt the bitstream.
//!
//! Much of this module is consumed only by later encoder milestones; the
//! `allow(dead_code)` markers below keep the crate warning-free until then.

#![allow(dead_code)] // consumed in later milestones

pub mod bit_buffer;
pub mod block;
pub mod bool_coder;
pub mod generated;
pub mod quant;
pub mod trees;

#[cfg(test)]
mod generated_tests;
