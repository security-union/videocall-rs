use anyhow::Result;
use image::{ImageBuffer, Rgb};
use std::fs;
use videocall_codecs::decoder::{Decodable, DecodedFrame, Decoder, VideoCodec};
use videocall_codecs::encoder::Vp9Encoder;
use videocall_codecs::frame::{FrameType, VideoFrame};
use videocall_codecs::jitter_buffer::JitterBuffer;

// Use the full vpx_sys crate
use vpx_sys::*;

// --- Main Simulation Logic ---

/// Converts an RGB buffer to a planar I420 YUV buffer.
/// This implementation is taken directly from the videocall-cli reference project
/// to ensure compatibility with the encoder.
fn rgb_to_i420(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let mut i420 = vec![0u8; width * height * 3 / 2];

    let (y_plane, uv_planes) = i420.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_planes.split_at_mut(width * height / 4);

    for y in 0..height {
        for x in 0..width {
            let rgb_index = (y * width + x) * 3;
            let r = rgb[rgb_index] as f32;
            let g = rgb[rgb_index + 1] as f32;
            let b = rgb[rgb_index + 2] as f32;

            // Calculate Y, U, V components using the standard formula.
            let y_value = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).round() as u8;
            let u_value = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).round() as u8;
            let v_value = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).round() as u8;

            y_plane[y * width + x] = y_value;

            if y % 2 == 0 && x % 2 == 0 {
                let uv_index = (y / 2) * (width / 2) + (x / 2);
                u_plane[uv_index] = u_value;
                v_plane[uv_index] = v_value;
            }
        }
    }
    i420
}

/// Converts an I420 planar YUV buffer to an RGB ImageBuffer.
/// This is the correct inverse of the `rgb_to_i420` function.
fn i420_to_rgb(yuv_data: &[u8], width: u32, height: u32) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    let mut rgb_image = ImageBuffer::new(width, height);
    let y_plane_size = (width * height) as usize;
    let uv_plane_size = (width * height / 4) as usize;

    let y_plane = &yuv_data[0..y_plane_size];
    let u_plane = &yuv_data[y_plane_size..(y_plane_size + uv_plane_size)];
    let v_plane = &yuv_data[(y_plane_size + uv_plane_size)..];

    for y in 0..height {
        for x in 0..width {
            let y_idx = (y * width + x) as usize;
            let uv_idx = ((y / 2) * (width / 2) + (x / 2)) as usize;

            let y_val = y_plane[y_idx] as f32;
            let u_val = u_plane[uv_idx] as f32;
            let v_val = v_plane[uv_idx] as f32;

            // YUV to RGB conversion based on the inverse of the rgb_to_i420 matrix.
            let c = y_val - 16.0;
            let d = u_val - 128.0;
            let e = v_val - 128.0;

            let r = (1.164 * c + 1.596 * e).round().clamp(0.0, 255.0) as u8;
            let g = (1.164 * c - 0.813 * e - 0.392 * d)
                .round()
                .clamp(0.0, 255.0) as u8;
            let b = (1.164 * c + 2.017 * d).round().clamp(0.0, 255.0) as u8;

            rgb_image.put_pixel(x, y, Rgb([r, g, b]));
        }
    }
    rgb_image
}

fn main() -> Result<()> {
    // Set a panic hook to catch panics from any thread
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("!!!!!!!! THREAD PANIC !!!!!!!");
        eprintln!("Panic info: {:?}", panic_info);
    }));

    // Create output directory
    fs::create_dir_all("output")?;

    println!("--- Encoder -> JitterBuffer -> Decoder Pipeline Test ---");

    // --- Simulation Setup ---

    // get width and height from the first image
    let img = image::open("assets/images/sample_video_save/sample_video_save/output_120.jpg")?;
    let width = img.width();
    let height = img.height();

    let mut encoder = Vp9Encoder::new(width, height, 500)?;
    let mut current_time_ms: u128 = 0;
    let clock_increment_ms: u128 = 33; // ~30 fps

    // --- Main Simulation Loop ---
    let on_decoded_frame = {
        let width = encoder.width;
        let height = encoder.height;
        move |frame: DecodedFrame| {
            println!(
                "[MAIN] DECODED FRAME RECEIVED! Seq: {}, Size: {}",
                frame.sequence_number,
                frame.data.len()
            );

            // Convert the I420 data back to RGB for saving.
            let rgb_image = i420_to_rgb(&frame.data, width, height);

            let output_path = format!("output/decoded_frame_{}.png", frame.sequence_number);
            if let Err(e) = rgb_image.save(&output_path) {
                eprintln!("[MAIN] Error saving frame: {}", e);
            } else {
                println!("[MAIN] Saved decoded frame to {}", output_path);
            }
        }
    };
    let decoder = Decoder::new(VideoCodec::VP9, Box::new(on_decoded_frame));
    let mut jitter_buffer =
        JitterBuffer::<videocall_codecs::decoder::DecodedFrame>::new(Box::new(decoder));

    // 2. Load real image frames from disk
    println!("Loading YUV frames from disk...");
    let yuv_frames = {
        let mut frames = Vec::new();
        let paths = std::fs::read_dir("assets/images/sample_video_save/sample_video_save")?;
        let mut sorted_paths: Vec<_> = paths.filter_map(Result::ok).collect();
        sorted_paths.sort_by_key(|a| a.path());

        for entry in sorted_paths {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jpg") {
                println!("[MAIN] Loading image: {:?}", path);
                let img = image::open(&path)?.to_rgb8();
                // Convert to I420 format for the encoder.
                frames.push(rgb_to_i420(img.as_raw(), img.width(), img.height()));
            }
        }
        frames
    };
    if yuv_frames.is_empty() {
        anyhow::bail!("No JPG images found in assets directory");
    }
    println!("[MAIN] Loaded {} YUV frames.", yuv_frames.len());

    // 3. Run the simulation loop for 300 frames, cycling through the loaded images.
    for i in 0..300 {
        let yuv_frame = &yuv_frames[i % yuv_frames.len()];
        current_time_ms += clock_increment_ms;
        let sequence_number = i as u64;

        // --- ENCODE ---
        let flags = if sequence_number % 10 == 0 {
            VPX_EFLAG_FORCE_KF as i64
        } else {
            0
        };
        let encoded_frames = encoder.encode(sequence_number as i64, Some(yuv_frame))?;
        println!(
            "[ENCODER] Frame {}, Size: {}, Keyframe: {}",
            sequence_number,
            yuv_frame.len(),
            flags != 0
        );

        // --- INSERT INTO JITTER BUFFER ---
        for frame in encoded_frames {
            let video_frame = VideoFrame {
                sequence_number: frame.pts as u64,
                frame_type: if frame.key {
                    FrameType::KeyFrame
                } else {
                    FrameType::DeltaFrame
                },
                data: frame.data.to_vec(),
                timestamp: current_time_ms as f64,
            };
            println!(
                "[JB_INSERT] Inserting Frame: {}, Type: {:?}, Size: {}",
                video_frame.sequence_number,
                video_frame.frame_type,
                video_frame.data.len()
            );
            jitter_buffer.insert_frame(video_frame, current_time_ms);
        }

        // --- POLL JITTER BUFFER FOR DECODABLE FRAMES ---
        jitter_buffer.find_and_move_continuous_frames(current_time_ms);

        // Simulate a delay between frames
        std::thread::sleep(std::time::Duration::from_millis(clock_increment_ms as u64));
    }

    // Final check to drain the buffer
    println!("[MAIN] Flushing encoder and draining jitter buffer...");
    let encoded_frames = encoder.encode(0, None)?; // Flush the encoder
    for frame in encoded_frames {
        let video_frame = VideoFrame {
            sequence_number: frame.pts as u64,
            frame_type: if frame.key {
                FrameType::KeyFrame
            } else {
                FrameType::DeltaFrame
            },
            data: frame.data.to_vec(),
            timestamp: current_time_ms as f64,
        };
        jitter_buffer.insert_frame(video_frame, current_time_ms);
    }

    current_time_ms += 200; // Add extra time to ensure last frames can be played out
    jitter_buffer.find_and_move_continuous_frames(current_time_ms);

    println!("[MAIN] Simulation finished.");
    Ok(())
}
