/*
 * Copyright 2022 l1npengtul <l1npengtul@protonmail.com> / The Nokhwa Contributors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
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
