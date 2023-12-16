use image::Rgb;
use nokhwa::{
    utils::{color_frame_formats, FrameFormat, Resolution},
    FormatDecoder, NokhwaError,
};

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
        _resolution: Resolution,
        data: &[u8],
    ) -> Result<Vec<u8>, NokhwaError> {
        match fcc {
            FrameFormat::YUYV => Ok(data.to_vec()),
            _ => Err(NokhwaError::GeneralError("Invalid FrameFormat".into())),
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
            FrameFormat::YUYV => {
                dest.copy_from_slice(data);
                Ok(())
            }
            _ => Err(NokhwaError::GeneralError("Invalid FrameFormat".into())),
        }
    }
}
