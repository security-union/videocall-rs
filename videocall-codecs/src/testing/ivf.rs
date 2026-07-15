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

//! A minimal IVF (DKIF) container writer.
//!
//! IVF is the simplest container `vpxdec`/`ffprobe` accept, so failing oracle
//! streams can be dumped to disk and inspected with standard tools. Only VP9
//! (`VP90`) is emitted.

use std::io::{self, Write};

/// Build the 32-byte IVF (DKIF) file header.
///
/// Layout (all multi-byte fields little-endian):
/// `DKIF`, version(0), header length(32), `VP90`, width, height,
/// time-base denominator (`fps`), time-base numerator (1), frame count, reserved.
pub fn ivf_header(width: u16, height: u16, fps: u32, num_frames: u32) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0..4].copy_from_slice(b"DKIF");
    h[4..6].copy_from_slice(&0u16.to_le_bytes()); // version
    h[6..8].copy_from_slice(&32u16.to_le_bytes()); // header length
    h[8..12].copy_from_slice(b"VP90"); // fourcc
    h[12..14].copy_from_slice(&width.to_le_bytes());
    h[14..16].copy_from_slice(&height.to_le_bytes());
    h[16..20].copy_from_slice(&fps.to_le_bytes()); // time base denominator
    h[20..24].copy_from_slice(&1u32.to_le_bytes()); // time base numerator
    h[24..28].copy_from_slice(&num_frames.to_le_bytes());
    // h[28..32] reserved, left zero.
    h
}

/// Streaming IVF writer wrapping any [`Write`].
///
/// The header is written up front with a frame count of zero; `vpxdec` and
/// `ffprobe` read frames until EOF, so a placeholder count is fine for debug
/// dumps.
pub struct IvfWriter<W: Write> {
    inner: W,
}

impl<W: Write> IvfWriter<W> {
    /// Create a writer and emit the 32-byte header immediately.
    pub fn new(mut inner: W, width: u16, height: u16, fps: u32) -> io::Result<Self> {
        inner.write_all(&ivf_header(width, height, fps, 0))?;
        Ok(Self { inner })
    }

    /// Append one frame: a 12-byte header (`size` u32 LE, `pts` u64 LE) then payload.
    pub fn write_frame(&mut self, data: &[u8], pts: u64) -> io::Result<()> {
        let mut fh = [0u8; 12];
        fh[0..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        fh[4..12].copy_from_slice(&pts.to_le_bytes());
        self.inner.write_all(&fh)?;
        self.inner.write_all(data)?;
        Ok(())
    }

    /// Flush and return the underlying writer.
    pub fn into_inner(mut self) -> io::Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_bytes_are_correct() {
        let h = ivf_header(640, 480, 30, 0);
        assert_eq!(&h[0..4], b"DKIF");
        assert_eq!(u16::from_le_bytes([h[4], h[5]]), 0); // version
        assert_eq!(u16::from_le_bytes([h[6], h[7]]), 32); // header length
        assert_eq!(&h[8..12], b"VP90");
        assert_eq!(u16::from_le_bytes([h[12], h[13]]), 640);
        assert_eq!(u16::from_le_bytes([h[14], h[15]]), 480);
        assert_eq!(u32::from_le_bytes([h[16], h[17], h[18], h[19]]), 30);
        assert_eq!(u32::from_le_bytes([h[20], h[21], h[22], h[23]]), 1);
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 0);
    }

    #[test]
    fn write_frame_emits_header_then_payload() {
        let mut out = Vec::new();
        {
            let mut w = IvfWriter::new(&mut out, 2, 2, 30).unwrap();
            w.write_frame(&[0xAA, 0xBB, 0xCC], 7).unwrap();
        }
        // 32-byte file header + 12-byte frame header + 3-byte payload.
        assert_eq!(out.len(), 32 + 12 + 3);
        let fh = &out[32..44];
        assert_eq!(u32::from_le_bytes([fh[0], fh[1], fh[2], fh[3]]), 3);
        assert_eq!(
            u64::from_le_bytes([fh[4], fh[5], fh[6], fh[7], fh[8], fh[9], fh[10], fh[11]]),
            7
        );
        assert_eq!(&out[44..47], &[0xAA, 0xBB, 0xCC]);
    }
}
