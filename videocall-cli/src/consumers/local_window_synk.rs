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

use crate::cli_args::Stream;

use super::camera_synk::CameraSynk;

pub struct LocalWindowSynk {}

impl LocalWindowSynk {
    pub fn new(_opts: Stream) -> LocalWindowSynk {
        LocalWindowSynk {}
    }
}

impl CameraSynk for LocalWindowSynk {
    async fn connect(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send_packet(&self, _data: Vec<u8>) -> anyhow::Result<()> {
        Ok(())
    }
}
