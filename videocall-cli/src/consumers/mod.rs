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

use camera_synk::CameraSynk;
use dead_synk::DeadSynk;
use webtransport::WebTransportClient;

pub mod camera_synk;
pub mod dead_synk;
pub mod local_window_synk;
pub mod webtransport;

pub enum CameraSynks {
    DeadSynk(DeadSynk),
    CameraSynk(Box<WebTransportClient>),
    // LocalWindowSynk(LocalWindowSynk)
}

impl CameraSynk for CameraSynks {
    async fn connect(&mut self) -> anyhow::Result<()> {
        match self {
            CameraSynks::DeadSynk(dead_synk) => dead_synk.connect().await,
            CameraSynks::CameraSynk(client) => client.connect().await,
        }
    }

    async fn send_packet(&self, data: Vec<u8>) -> anyhow::Result<()> {
        match self {
            CameraSynks::CameraSynk(client) => client.send_packet(data).await,
            CameraSynks::DeadSynk(dead_synk) => dead_synk.send_packet(data).await,
        }
    }
}
