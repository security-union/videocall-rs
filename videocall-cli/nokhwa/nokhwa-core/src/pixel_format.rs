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

use crate::error::NokhwaError;
use crate::types::{
    buf_bgr_to_rgb, buf_mjpeg_to_rgb, buf_nv12_to_rgb, buf_yuyv422_to_rgb, color_frame_formats,
    frame_formats, mjpeg_to_rgb, nv12_to_i420, nv12_to_rgb, yuyv422_to_rgb, FrameFormat,
    Resolution,
};
use image::{Luma, LumaA, Pixel, Rgb, Rgba};
use std::fmt::Debug;

/// Trait that has methods to convert raw data from the webcam to a proper raw image.
pub trait FormatDecoder: Clone + Sized + Send + Sync {
    type Output: Pixel<Subpixel = u8>;
    const FORMATS: &'static [FrameFormat];

    /// Allocates and returns a `Vec`
    /// # Errors
    /// If the data is malformed, or the source [`FrameFormat`] is incompatible, this will error.
    fn write_output(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError>;

    /// Writes to a user provided buffer.
    /// # Errors
    /// If the data is malformed, the source [`FrameFormat`] is incompatible, or the user-alloted buffer is not large enough, this will error.
    fn write_output_buffer(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
        dest: &mut [u8],
    ) -> Result<(), NokhwaError>;
}

/// A Zero-Size-Type that contains the definition to convert a given image stream to an RGB888 in the [`Buffer`](crate::buffer::Buffer)'s [`.decode_image()`](crate::buffer::Buffer::decode_image)
///
/// ```.ignore
/// use image::{ImageBuffer, Rgb};
/// let image: ImageBuffer<Rgb<u8>, Vec<u8>> = buffer.to_image::<RgbFormat>();
/// ```
#[derive(Copy, Clone, Debug, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct RgbFormat;

impl FormatDecoder for RgbFormat {
    type Output = Rgb<u8>;
    const FORMATS: &'static [FrameFormat] = color_frame_formats();

    #[inline]
    fn write_output(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => mjpeg_to_rgb(data, false),
            FrameFormat::YUYV => yuyv422_to_rgb(data, false),
            FrameFormat::GRAY => Ok(data
                .iter()
                .flat_map(|x| {
                    let pxv = *x;
                    [pxv, pxv, pxv]
                })
                .collect()),
            FrameFormat::RAWRGB => Ok(data.to_vec()),
            FrameFormat::RAWBGR => {
                let mut rgb = vec![0u8; data.len()];
                data.chunks_exact(3).enumerate().for_each(|(idx, px)| {
                    let index = idx * 3;
                    rgb[index] = px[2];
                    rgb[index + 1] = px[1];
                    rgb[index + 2] = px[0];
                });
                Ok(rgb)
            }
            FrameFormat::NV12 => nv12_to_rgb(resolution, data, false),
        }
    }

    #[inline]
    fn write_output_buffer(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
        dest: &mut [u8],
    ) -> Result<(), NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => buf_mjpeg_to_rgb(data, dest, false),
            FrameFormat::YUYV => buf_yuyv422_to_rgb(data, dest, false),
            FrameFormat::GRAY => {
                if dest.len() != data.len() * 3 {
                    return Err(NokhwaError::ProcessFrameError {
                        src: fcc,
                        destination: "Luma => RGB".to_string(),
                        error: "Bad buffer length".to_string(),
                    });
                }

                data.iter().enumerate().for_each(|(idx, pixel_value)| {
                    let index = idx * 3;
                    dest[index] = *pixel_value;
                    dest[index + 1] = *pixel_value;
                    dest[index + 2] = *pixel_value;
                });
                Ok(())
            }
            FrameFormat::RAWRGB => {
                dest.copy_from_slice(data);
                Ok(())
            }
            FrameFormat::RAWBGR => buf_bgr_to_rgb(resolution, data, dest),
            FrameFormat::NV12 => buf_nv12_to_rgb(resolution, data, dest, false),
        }
    }
}

/// A Zero-Size-Type that contains the definition to convert a given image stream to an RGBA8888 in the [`Buffer`](crate::buffer::Buffer)'s [`.decode_image()`](crate::buffer::Buffer::decode_image)
///
/// ```.ignore
/// use image::{ImageBuffer, Rgba};
/// let image: ImageBuffer<Rgba<u8>, Vec<u8>> = buffer.to_image::<RgbAFormat>();
/// ```
#[derive(Copy, Clone, Debug, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct RgbAFormat;

impl FormatDecoder for RgbAFormat {
    type Output = Rgba<u8>;

    const FORMATS: &'static [FrameFormat] = color_frame_formats();

    #[inline]
    fn write_output(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => mjpeg_to_rgb(data, true),
            FrameFormat::YUYV => yuyv422_to_rgb(data, true),
            FrameFormat::GRAY => Ok(data
                .iter()
                .flat_map(|x| {
                    let pxv = *x;
                    [pxv, pxv, pxv, 255]
                })
                .collect()),
            FrameFormat::RAWRGB => Ok(data
                .chunks_exact(3)
                .flat_map(|x| [x[0], x[1], x[2], 255])
                .collect()),
            FrameFormat::RAWBGR => Ok(data
                .chunks_exact(3)
                .flat_map(|x| [x[2], x[1], x[0], 255])
                .collect()),
            FrameFormat::NV12 => nv12_to_rgb(resolution, data, true),
        }
    }

    #[inline]
    fn write_output_buffer(
        fcc: FrameFormat,
        resolution: Resolution,

        data: &[u8],
        dest: &mut [u8],
    ) -> Result<(), NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => buf_mjpeg_to_rgb(data, dest, true),
            FrameFormat::YUYV => buf_yuyv422_to_rgb(data, dest, true),
            FrameFormat::GRAY => {
                if dest.len() != data.len() * 4 {
                    return Err(NokhwaError::ProcessFrameError {
                        src: fcc,
                        destination: "Luma => RGBA".to_string(),
                        error: "Bad buffer length".to_string(),
                    });
                }

                data.iter().enumerate().for_each(|(idx, pixel_value)| {
                    let index = idx * 4;
                    dest[index] = *pixel_value;
                    dest[index + 1] = *pixel_value;
                    dest[index + 2] = *pixel_value;
                    dest[index + 3] = 255;
                });
                Ok(())
            }
            FrameFormat::RAWRGB => {
                data.chunks_exact(3).enumerate().for_each(|(idx, px)| {
                    let index = idx * 4;
                    dest[index] = px[0];
                    dest[index + 1] = px[1];
                    dest[index + 2] = px[2];
                    dest[index + 3] = 255;
                });
                Ok(())
            }
            FrameFormat::RAWBGR => {
                data.chunks_exact(3).enumerate().for_each(|(idx, px)| {
                    let index = idx * 4;
                    dest[index] = px[2];
                    dest[index + 1] = px[1];
                    dest[index + 2] = px[0];
                    dest[index + 3] = 255;
                });
                Ok(())
            }
            FrameFormat::NV12 => buf_nv12_to_rgb(resolution, data, dest, true),
        }
    }
}

/// A Zero-Size-Type that contains the definition to convert a given image stream to an Luma8(Grayscale 8-bit) in the [`Buffer`](crate::buffer::Buffer)'s [`.decode_image()`](crate::buffer::Buffer::decode_image)
///
/// ```.ignore
/// use image::{ImageBuffer, Luma};
/// let image: ImageBuffer<Luma<u8>, Vec<u8>> = buffer.to_image::<LumaFormat>();
/// ```
#[derive(Copy, Clone, Debug, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct LumaFormat;

impl FormatDecoder for LumaFormat {
    type Output = Luma<u8>;

    const FORMATS: &'static [FrameFormat] = frame_formats();

    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    #[inline]
    fn write_output(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => Ok(mjpeg_to_rgb(data, false)?
                .as_slice()
                .chunks_exact(3)
                .map(|x| {
                    let mut avg = 0;
                    x.iter().for_each(|v| avg += u16::from(*v));
                    (avg / 3) as u8
                })
                .collect()),
            FrameFormat::YUYV => Ok(yuyv422_to_rgb(data, false)?
                .as_slice()
                .chunks_exact(3)
                .map(|x| {
                    let mut avg = 0;
                    x.iter().for_each(|v| avg += u16::from(*v));
                    (avg / 3) as u8
                })
                .collect()),
            FrameFormat::NV12 => Ok(nv12_to_rgb(resolution, data, false)?
                .as_slice()
                .chunks_exact(3)
                .map(|x| {
                    let mut avg = 0;
                    x.iter().for_each(|v| avg += u16::from(*v));
                    (avg / 3) as u8
                })
                .collect()),
            FrameFormat::GRAY => Ok(data.to_vec()),
            FrameFormat::RAWRGB => Ok(data
                .chunks(3)
                .map(|px| ((i32::from(px[0]) + i32::from(px[1]) + i32::from(px[2])) / 3) as u8)
                .collect()),
            FrameFormat::RAWBGR => Ok(data
                .chunks(3)
                .map(|px| ((i32::from(px[2]) + i32::from(px[1]) + i32::from(px[0])) / 3) as u8)
                .collect()),
        }
    }

    #[inline]
    fn write_output_buffer(
        fcc: FrameFormat,
        _resolution: Resolution,
        data: &[u8],
        dest: &mut [u8],
    ) -> Result<(), NokhwaError> {
        match fcc {
            // TODO: implement!
            FrameFormat::MJPEG | FrameFormat::YUYV | FrameFormat::NV12 => {
                Err(NokhwaError::ProcessFrameError {
                    src: fcc,
                    destination: "RGB => Luma".to_string(),
                    error: "Conversion Error".to_string(),
                })
            }
            FrameFormat::GRAY => {
                data.iter().zip(dest.iter_mut()).for_each(|(pxv, d)| {
                    *d = *pxv;
                });
                Ok(())
            }
            FrameFormat::RAWRGB => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "RGB => Luma".to_string(),
                error: "Conversion Error".to_string(),
            }),
            FrameFormat::RAWBGR => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "BGR => Luma".to_string(),
                error: "Conversion Error".to_string(),
            }),
        }
    }
}

/// A Zero-Size-Type that contains the definition to convert a given image stream to an LumaA8(Grayscale 8-bit with 8-bit alpha) in the [`Buffer`](crate::buffer::Buffer)'s [`.decode_image()`](crate::buffer::Buffer::decode_image)
///
/// ```.ignore
/// use image::{ImageBuffer, LumaA};
/// let image: ImageBuffer<LumaA<u8>, Vec<u8>> = buffer.to_image::<LumaAFormat>();
/// ```
#[derive(Copy, Clone, Debug, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct LumaAFormat;

impl FormatDecoder for LumaAFormat {
    type Output = LumaA<u8>;

    const FORMATS: &'static [FrameFormat] = frame_formats();

    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    fn write_output(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => Ok(mjpeg_to_rgb(data, false)?
                .as_slice()
                .chunks_exact(3)
                .flat_map(|x| {
                    let mut avg = 0;
                    x.iter().for_each(|v| avg += u16::from(*v));
                    [(avg / 3) as u8, 255]
                })
                .collect()),
            FrameFormat::YUYV => Ok(yuyv422_to_rgb(data, false)?
                .as_slice()
                .chunks_exact(3)
                .flat_map(|x| {
                    let mut avg = 0;
                    x.iter().for_each(|v| avg += u16::from(*v));
                    [(avg / 3) as u8, 255]
                })
                .collect()),
            FrameFormat::NV12 => Ok(nv12_to_rgb(resolution, data, false)?
                .as_slice()
                .chunks_exact(3)
                .flat_map(|x| {
                    let mut avg = 0;
                    x.iter().for_each(|v| avg += u16::from(*v));
                    [(avg / 3) as u8, 255]
                })
                .collect()),
            FrameFormat::GRAY => Ok(data.iter().flat_map(|x| [*x, 255]).collect()),
            FrameFormat::RAWRGB => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "RGB => LumaA".to_string(),
                error: "Conversion Error".to_string(),
            }),
            FrameFormat::RAWBGR => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "BGR => LumaA".to_string(),
                error: "Conversion Error".to_string(),
            }),
        }
    }

    #[inline]
    fn write_output_buffer(
        fcc: FrameFormat,
        _resolution: Resolution,
        data: &[u8],
        dest: &mut [u8],
    ) -> Result<(), NokhwaError> {
        match fcc {
            FrameFormat::MJPEG => {
                // FIXME: implement!
                Err(NokhwaError::ProcessFrameError {
                    src: fcc,
                    destination: "MJPEG => LumaA".to_string(),
                    error: "Conversion Error".to_string(),
                })
            }
            FrameFormat::YUYV => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "YUYV => LumaA".to_string(),
                error: "Conversion Error".to_string(),
            }),
            FrameFormat::NV12 => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "NV12 => LumaA".to_string(),
                error: "Conversion Error".to_string(),
            }),
            FrameFormat::GRAY => {
                if dest.len() != data.len() * 2 {
                    return Err(NokhwaError::ProcessFrameError {
                        src: fcc,
                        destination: "GRAY8 => LumaA".to_string(),
                        error: "Conversion Error".to_string(),
                    });
                }

                data.iter()
                    .zip(dest.chunks_exact_mut(2))
                    .enumerate()
                    .for_each(|(idx, (pxv, d))| {
                        let index = idx * 2;
                        d[index] = *pxv;
                        d[index + 1] = 255;
                    });
                Ok(())
            }
            FrameFormat::RAWRGB => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "RGB => LumaA".to_string(),
                error: "Conversion Error".to_string(),
            }),
            FrameFormat::RAWBGR => Err(NokhwaError::ProcessFrameError {
                src: fcc,
                destination: "BGR => LumaA".to_string(),
                error: "Conversion Error".to_string(),
            }),
        }
    }
}

/// let image: ImageBuffer<Rgb<u8>, Vec<u8>> = buffer.to_image::<I420Format>();
/// ```
#[derive(Copy, Clone, Debug, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct I420Format;

impl FormatDecoder for I420Format {
    type Output = Rgb<u8>;
    const FORMATS: &'static [FrameFormat] = color_frame_formats();

    #[inline]
    fn write_output(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError> {
        match fcc {
            FrameFormat::YUYV => {
                let i420 = private_convert_yuyv_to_i420(
                    data,
                    resolution.width() as usize,
                    resolution.height() as usize,
                );
                Ok(i420)
            }
            _ => Err(NokhwaError::GeneralError("Invalid FrameFormat".into())),
        }
    }

    #[inline]
    fn write_output_buffer(
        fcc: FrameFormat,
        resolution: Resolution,
        data: &[u8],
        dest: &mut [u8],
    ) -> Result<(), NokhwaError> {
        match fcc {
            FrameFormat::YUYV => {
                convert_yuyv_to_i420_direct(
                    data,
                    dest,
                    resolution.width() as usize,
                    resolution.height() as usize,
                )?;
                Ok(())
            }
            FrameFormat::NV12 => nv12_to_i420(
                data,
                resolution.width() as usize,
                resolution.height() as usize,
                dest,
            ),
            _ => Err(NokhwaError::GeneralError("Invalid FrameFormat".into())),
        }
    }
}

fn private_convert_yuyv_to_i420(yuyv: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert!(
        width % 2 == 0 && height % 2 == 0,
        "Width and height must be even numbers."
    );

    let mut i420 = vec![0u8; width * height + 2 * (width / 2) * (height / 2)];
    let (y_plane, uv_plane) = i420.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_plane.split_at_mut(uv_plane.len() / 2);

    for y in 0..height {
        for x in (0..width).step_by(2) {
            let base_index = (y * width + x) * 2;
            let y0 = yuyv[base_index];
            let u = yuyv[base_index + 1];
            let y1 = yuyv[base_index + 2];
            let v = yuyv[base_index + 3];

            y_plane[y * width + x] = y0;
            y_plane[y * width + x + 1] = y1;

            if y % 2 == 0 {
                u_plane[y / 2 * (width / 2) + x / 2] = u;
                v_plane[y / 2 * (width / 2) + x / 2] = v;
            }
        }
    }

    i420
}

fn convert_yuyv_to_i420_direct(
    yuyv: &[u8],
    dest: &mut [u8],
    width: usize,
    height: usize,
) -> Result<(), NokhwaError> {
    // Ensure the source buffer holds a full YUYV frame (2 bytes/pixel). Without
    // this, a frame mis-tagged as YUYV (or genuinely YUYV but shorter than its
    // resolution implies) would index out of bounds below and panic.
    if yuyv.len() < width * height * 2 {
        return Err(NokhwaError::GeneralError(
            "YUYV source buffer is too small".into(),
        ));
    }

    // Ensure the destination buffer is large enough
    if dest.len() < width * height + 2 * (width / 2) * (height / 2) {
        return Err(NokhwaError::GeneralError(
            "Destination buffer is too small".into(),
        ));
    }

    // Split the destination buffer into Y, U, and V planes
    let (y_plane, uv_plane) = dest.split_at_mut(width * height);
    let (u_plane, v_plane) = uv_plane.split_at_mut(uv_plane.len() / 2);

    // Convert YUYV to I420
    for y in 0..height {
        for x in (0..width).step_by(2) {
            let base_index = (y * width + x) * 2;
            let y0 = yuyv[base_index];
            let u = yuyv[base_index + 1];
            let y1 = yuyv[base_index + 2];
            let v = yuyv[base_index + 3];

            y_plane[y * width + x] = y0;
            y_plane[y * width + x + 1] = y1;

            if y % 2 == 0 {
                u_plane[y / 2 * (width / 2) + x / 2] = u;
                v_plane[y / 2 * (width / 2) + x / 2] = v;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::I420Format;
    use crate::buffer::Buffer;
    use crate::types::{FrameFormat, Resolution};

    // Regression: an external/virtual camera (e.g. OBS) can deliver frames at a
    // resolution different from the one the consumer sized its I420 buffer for.
    // Decoding must return an error, never panic in `nv12_to_i420`'s `split_at`.

    #[test]
    fn nv12_decode_short_input_errors_not_panics() {
        // A 4x4 NV12 frame needs 4*4*3/2 = 24 bytes; supply only 10.
        let buffer = Buffer::new_from_vec(Resolution::new(4, 4), vec![0u8; 10], FrameFormat::NV12);
        let mut dest = vec![0u8; 24];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(result.is_err(), "short NV12 input must error, not panic");
    }

    #[test]
    fn nv12_decode_short_dest_errors_not_panics() {
        // Correctly sized 4x4 input, but the destination is too small (10 < 24) —
        // exactly the live-stream case where the frame is larger than the buffer.
        let buffer = Buffer::new_from_vec(Resolution::new(4, 4), vec![0u8; 24], FrameFormat::NV12);
        let mut dest = vec![0u8; 10];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(
            result.is_err(),
            "undersized I420 destination must error, not panic"
        );
    }

    #[test]
    fn nv12_decode_matched_sizes_succeeds() {
        // The happy path still works: matched input, dest, and even dimensions.
        let buffer = Buffer::new_from_vec(Resolution::new(4, 4), vec![0u8; 24], FrameFormat::NV12);
        let mut dest = vec![0u8; 24];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(result.is_ok(), "matched NV12 decode must succeed");
    }

    // Mis-tagged-format cases: the delivered pixel format (byte layout) does not
    // match the FrameFormat the Buffer is tagged with — the historically likely
    // cause of this panic class. Decoding must always error or succeed cleanly,
    // never panic on an out-of-bounds index.

    #[test]
    fn nv12_tagged_but_gray_sized_input_errors_not_panics() {
        // 8x8 tagged NV12 needs 8*8*3/2 = 96 bytes; a GRAY frame is only 8*8 = 64.
        let buffer = Buffer::new_from_vec(Resolution::new(8, 8), vec![0u8; 64], FrameFormat::NV12);
        let mut dest = vec![0u8; 96];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(
            result.is_err(),
            "NV12-tagged GRAY-sized input must error, not panic"
        );
    }

    #[test]
    fn yuyv_tagged_but_nv12_sized_input_errors_not_panics() {
        // 8x8 tagged YUYV needs 8*8*2 = 128 bytes; an NV12 frame is only 96.
        let buffer = Buffer::new_from_vec(Resolution::new(8, 8), vec![0u8; 96], FrameFormat::YUYV);
        let mut dest = vec![0u8; 96];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(
            result.is_err(),
            "YUYV-tagged NV12-sized input must error, not panic"
        );
    }

    #[test]
    fn nv12_tagged_but_larger_input_does_not_panic() {
        // Tagged NV12 but carrying a YUYV-sized (2 bytes/px) frame: larger than
        // required, so decoding the leading bytes is safe — must not panic.
        let buffer = Buffer::new_from_vec(Resolution::new(8, 8), vec![0u8; 128], FrameFormat::NV12);
        let mut dest = vec![0u8; 96];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(
            result.is_ok(),
            "oversized NV12 input must decode without panic"
        );
    }

    #[test]
    fn nv12_decode_odd_width_errors_not_panics() {
        // NV12's 4:2:0 chroma subsampling assumes even dimensions. An odd width
        // must be rejected up front rather than producing desynced plane math.
        // Buffers are sized generously (5*4*2 = 40) so the ONLY reason to error
        // is the odd dimension, not a size mismatch.
        let buffer = Buffer::new_from_vec(Resolution::new(5, 4), vec![0u8; 40], FrameFormat::NV12);
        let mut dest = vec![0u8; 40];
        let result = buffer.decode_image_to_buffer::<I420Format>(&mut dest);
        assert!(result.is_err(), "odd width must error, not panic or desync");
    }
}
