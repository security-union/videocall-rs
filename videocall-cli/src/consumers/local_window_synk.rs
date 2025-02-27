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
