use image::color::Rgb;
use nokhwa::{
    utils::{color_frame_formats, FrameFormat, Resolution},
    FormatDecoder, NokhwaError,
};
use tracing::info;

/// let image: ImageBuffer<Rgb<u8>, Vec<u8>> = buffer.to_image::<YuyvFormat>();
/// ```
#[derive(Copy, Clone, Debug, Default, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct YuyvFormat;

impl FormatDecoder for YuyvFormat {
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
                info!("YUYV format");
                let i420 = convert_yuyv_to_i420(
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
            _ => Err(NokhwaError::GeneralError("Invalid FrameFormat".into())),
        }
    }
}

fn convert_yuyv_to_i420(yuyv: &[u8], width: usize, height: usize) -> Vec<u8> {
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
