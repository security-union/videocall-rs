use camera_synk::CameraSynk;
use dead_synk::DeadSynk;
use quic::Client;

pub mod camera_synk;
pub mod dead_synk;
pub mod quic;

pub enum CameraSynks {
    DeadSynk(DeadSynk),
    CameraSynk(Client),
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
