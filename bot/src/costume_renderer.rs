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

use anyhow::{anyhow, Context};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use tracing::info;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FRAME_BYTES: usize = (WIDTH as usize) * (HEIGHT as usize) * 3 / 2; // I420

pub struct CostumeRenderer {
    /// Read-only memory maps let the OS page cache manage residency and share
    /// the same costume blobs across multiple bot processes on a host.
    idle_frames: Mmap,
    talking_frames: Mmap,
    idle_count: usize,
    talking_count: usize,
}

impl CostumeRenderer {
    pub fn load(costume_dir: &Path) -> anyhow::Result<Self> {
        let idle_path = costume_dir.join("idle.i420");
        let talking_path = costume_dir.join("talking.i420");

        let idle_file = File::open(&idle_path)
            .with_context(|| format!("Failed to open {}", idle_path.display()))?;
        let talking_file = File::open(&talking_path)
            .with_context(|| format!("Failed to open {}", talking_path.display()))?;

        // SAFETY: The files are opened read-only and the resulting mappings
        // live for at most as long as the renderer. The costume assets are
        // immutable blobs, so no concurrent mutation invalidates the slices.
        let idle_frames = unsafe { Mmap::map(&idle_file) }
            .with_context(|| format!("Failed to mmap {}", idle_path.display()))?;
        let talking_frames = unsafe { Mmap::map(&talking_file) }
            .with_context(|| format!("Failed to mmap {}", talking_path.display()))?;

        if idle_frames.len() % FRAME_BYTES != 0 || idle_frames.is_empty() {
            return Err(anyhow!(
                "idle.i420 size {} is not a multiple of frame size {} ({}x{} I420)",
                idle_frames.len(),
                FRAME_BYTES,
                WIDTH,
                HEIGHT
            ));
        }
        if talking_frames.len() % FRAME_BYTES != 0 || talking_frames.is_empty() {
            return Err(anyhow!(
                "talking.i420 size {} is not a multiple of frame size {}",
                talking_frames.len(),
                FRAME_BYTES
            ));
        }

        let idle_count = idle_frames.len() / FRAME_BYTES;
        let talking_count = talking_frames.len() / FRAME_BYTES;

        info!(
            "Loaded costume from {}: {} idle frames, {} talking frames",
            costume_dir.display(),
            idle_count,
            talking_count
        );

        Ok(Self {
            idle_frames,
            talking_frames,
            idle_count,
            talking_count,
        })
    }

    pub fn frame_i420(&self, is_speaking: bool, frame_idx: usize) -> &[u8] {
        if is_speaking {
            let idx = frame_idx % self.talking_count;
            &self.talking_frames[idx * FRAME_BYTES..(idx + 1) * FRAME_BYTES]
        } else {
            let idx = frame_idx % self.idle_count;
            &self.idle_frames[idx * FRAME_BYTES..(idx + 1) * FRAME_BYTES]
        }
    }

    pub fn width(&self) -> u32 {
        WIDTH
    }

    pub fn height(&self) -> u32 {
        HEIGHT
    }
}
