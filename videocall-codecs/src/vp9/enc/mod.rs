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

//! Encoder-side VP9 logic (bitstream header packing for now; forward transforms,
//! mode decision, motion search and rate control land in later milestones).
//!
//! Most of this module is consumed only by later encoder milestones.

#![allow(dead_code)] // consumed in later milestones

pub mod bitstream;
pub mod block_encode;
pub mod encodemv;
pub mod encoder;
pub mod fdct;
pub mod mcomp;
pub mod pack;
pub mod quantize;
pub mod ratectrl;
pub mod speed;
pub mod tokenize;
