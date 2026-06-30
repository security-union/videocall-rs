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

use videocall_cli::cli_args::{IndexKind, Info};
use videocall_nokhwa::{
    native_api_backend,
    pixel_format::RgbFormat,
    query,
    utils::{frame_formats, CameraIndex, RequestedFormat, RequestedFormatType},
    Camera,
};

pub async fn get_info(info: Info) -> anyhow::Result<()> {
    if info.list_cameras {
        let backend = native_api_backend().unwrap();
        let devices = query(backend).unwrap();
        println!("There are {} available cameras.", devices.len());
        for device in devices {
            println!("{device}");
        }
    }

    if info.list_audio_devices {
        list_audio_devices()?;
    }

    if let Some(index) = info.list_formats {
        let index = match index {
            IndexKind::String(s) => CameraIndex::String(s.clone()),
            IndexKind::Index(i) => CameraIndex::Index(i),
        };
        let mut camera = Camera::new(
            index,
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
        )?;
        camera_compatible_formats(&mut camera);
    }

    Ok(())
}

/// List audio input devices via `cpal` (the same backend the microphone
/// producer uses). The printed names are exactly what `stream --audio-device`
/// expects; `--audio-device default` always selects the host default.
fn list_audio_devices() -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let devices: Vec<String> = host
        .input_devices()?
        .filter_map(|d| d.name().ok())
        .collect();

    println!("There are {} available audio input devices.", devices.len());
    for name in &devices {
        let marker = if Some(name) == default_name.as_ref() {
            " (default)"
        } else {
            ""
        };
        println!("{name}{marker}");
    }
    println!(
        "\nPass one of these to `stream --audio-device <NAME>`, \
         or use `--audio-device default`."
    );
    Ok(())
}

fn camera_compatible_formats(cam: &mut Camera) {
    println!("{}", cam.info());
    for ffmt in frame_formats() {
        if let Ok(compatible) = cam.compatible_list_by_resolution(*ffmt) {
            println!("{ffmt}:");
            let mut formats = Vec::new();
            for (resolution, fps) in compatible {
                formats.push((resolution, fps));
            }
            formats.sort_by(|a, b| a.0.cmp(&b.0));
            for fmt in formats {
                let (resolution, res) = fmt;
                println!(" - {resolution}: {res:?}")
            }
        }
    }
}
