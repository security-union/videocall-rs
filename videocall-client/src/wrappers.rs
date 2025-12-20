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

use std::fmt;
use web_sys::*;

pub struct EncodedVideoChunkTypeWrapper(pub EncodedVideoChunkType);

impl From<&str> for EncodedVideoChunkTypeWrapper {
    fn from(s: &str) -> Self {
        match s {
            "key" => EncodedVideoChunkTypeWrapper(EncodedVideoChunkType::Key),
            _ => EncodedVideoChunkTypeWrapper(EncodedVideoChunkType::Delta),
        }
    }
}

impl fmt::Display for EncodedVideoChunkTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            EncodedVideoChunkType::Delta => write!(f, "delta"),
            EncodedVideoChunkType::Key => write!(f, "key"),
            _ => todo!(),
        }
    }
}

pub struct EncodedAudioChunkTypeWrapper(pub EncodedAudioChunkType);

impl From<&str> for EncodedAudioChunkTypeWrapper {
    fn from(s: &str) -> Self {
        match s {
            "key" => EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Key),
            _ => EncodedAudioChunkTypeWrapper(EncodedAudioChunkType::Delta),
        }
    }
}

impl fmt::Display for EncodedAudioChunkTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            EncodedAudioChunkType::Delta => write!(f, "delta"),
            EncodedAudioChunkType::Key => write!(f, "key"),
            _ => todo!(),
        }
    }
}
