use crate::cli_args::Streaming;

use super::camera_synk::CameraSynk;

pub struct DeadSynk {}

impl DeadSynk {
    pub fn new(opts: Streaming) -> DeadSynk {
        DeadSynk {}
    }
}

impl CameraSynk for DeadSynk {
    async fn connect(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send_packet(&self, _data: Vec<u8>) -> anyhow::Result<()> {
        Ok(())
    }
}
