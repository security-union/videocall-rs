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

//! EKG-style video frame renderer.
//!
//! Generates animated waveform frames in memory (JPEG bytes) that visually
//! indicate when a participant is speaking vs listening. Ported from the
//! Python `render_frame()` function in generate-conversation-edge.py.

use image::{ImageBuffer, Rgb};
use std::f64::consts::PI;

const BG_COLOR: [u8; 3] = [20, 20, 30];
const GRID_COLOR: [u8; 3] = [40, 40, 55];
const FLAT_COLOR: [u8; 3] = [60, 60, 80];

pub struct EkgRenderer {
    color: [u8; 3],
    width: u32,
    height: u32,
}

impl EkgRenderer {
    pub fn new(color: [u8; 3], width: u32, height: u32) -> Self {
        Self {
            color,
            width,
            height,
        }
    }

    /// Render a single frame as an RGB ImageBuffer.
    /// Used by the video producer for on-the-fly rendering in the encode loop.
    /// Render a single frame into a pre-allocated ImageBuffer.
    ///
    /// Caller should create the buffer once via `EkgRenderer::create_frame_buffer()`
    /// and reuse it across frames to avoid per-frame heap allocation.
    pub fn render_frame_rgb_into(
        &self,
        img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
        rms_value: f32,
        max_rms: f32,
        frame_idx: usize,
    ) {
        let w = self.width as usize;
        let h = self.height as usize;
        let center_y = h / 2;

        // Clear to background (memset-like fill instead of per-pixel closure)
        let bg = Rgb(BG_COLOR);
        for pixel in img.pixels_mut() {
            *pixel = bg;
        }

        // Grid lines
        let mut dy: i32 = -300;
        while dy <= 300 {
            let y = (center_y as i32 + dy) as u32;
            if y < self.height {
                for x in 0..self.width {
                    img.put_pixel(x, y, Rgb(GRID_COLOR));
                }
            }
            dy += 60;
        }

        let is_speaking = rms_value > 0.01 && max_rms > 0.01;

        if is_speaking {
            let amplitude = (rms_value / max_rms).min(1.0) as f64;
            let wave_height = (amplitude * 280.0) as i32;
            let phase = frame_idx as f64 * 0.3;

            // Draw EKG wave — glow layer (wider, dimmer) then sharp layer
            let glow_color = Rgb([self.color[0] / 3, self.color[1] / 3, self.color[2] / 3]);

            let mut prev_y: Option<i32> = None;
            for x in 0..w {
                let t = x as f64 / w as f64 * 12.0 * PI + phase;
                let val = (t).sin() * 0.5
                    + (t * 2.3).sin() * 0.3
                    + (t * 5.7).sin() * 0.15
                    + (t * 0.7).sin() * 0.05;
                let y = center_y as i32 - (val * wave_height as f64) as i32;

                // Fill vertical gap between consecutive x positions (glow)
                if let Some(py) = prev_y {
                    let (y_min, y_max) = if py < y { (py, y) } else { (y, py) };
                    for fill_y in y_min..=y_max {
                        for dy_off in -2i32..=2 {
                            let fy = (fill_y + dy_off) as u32;
                            if fy < self.height {
                                img.put_pixel(x as u32, fy, glow_color);
                            }
                        }
                    }
                }

                // Sharp line (1px)
                if y >= 0 && (y as u32) < self.height {
                    img.put_pixel(x as u32, y as u32, Rgb(self.color));
                }
                prev_y = Some(y);
            }

            // Amplitude bar on right edge
            let bar_x = w as u32 - 40;
            let bar_top = (center_y as i32 - wave_height).max(0) as u32;
            let bar_bot = (center_y as i32 + wave_height).min(h as i32 - 1) as u32;
            for y in bar_top..=bar_bot {
                for x in bar_x..bar_x.saturating_add(20).min(self.width) {
                    img.put_pixel(x, y, Rgb(self.color));
                }
            }
        } else {
            // Flat line with travelling pulse
            let pulse_x = (frame_idx * 8) % w;
            for x in 0..w {
                let dist = (x as i32 - pulse_x as i32).unsigned_abs() as f64;
                let bump = if dist < 40.0 {
                    (8.0 * (-dist * dist / 200.0).exp()) as i32
                } else {
                    0
                };
                let y = (center_y as i32 - bump) as u32;
                if y < self.height {
                    img.put_pixel(x as u32, y, Rgb(FLAT_COLOR));
                }
            }
        }

    }

    /// Create a reusable frame buffer for `render_frame_rgb_into`.
    pub fn create_frame_buffer(&self) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        ImageBuffer::from_pixel(self.width, self.height, Rgb(BG_COLOR))
    }
}

/// Compute RMS energy per video frame from audio samples.
///
/// Returns a Vec of RMS values, one per frame. Applies a 0.3s moving-average
/// smoothing window so the EKG animation transitions smoothly.
pub fn compute_rms_per_frame(audio: &[f32], sample_rate: u32, fps: u32) -> Vec<f32> {
    let samples_per_frame = (sample_rate / fps) as usize;
    if samples_per_frame == 0 {
        return Vec::new();
    }
    let n_frames = audio.len() / samples_per_frame;
    let mut rms = Vec::with_capacity(n_frames);

    for i in 0..n_frames {
        let chunk = &audio[i * samples_per_frame..(i + 1) * samples_per_frame];
        let mean_sq: f32 = chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32;
        rms.push(mean_sq.sqrt());
    }

    // Smoothing: moving average over 0.3s window
    let smooth_frames = (0.3 * fps as f32) as usize;
    if smooth_frames > 1 && rms.len() > 1 {
        let mut smoothed = vec![0.0f32; rms.len()];
        for i in 0..rms.len() {
            let start = i.saturating_sub(smooth_frames / 2);
            let end = (i + smooth_frames / 2 + 1).min(rms.len());
            smoothed[i] = rms[start..end].iter().sum::<f32>() / (end - start) as f32;
        }
        return smoothed;
    }

    rms
}
