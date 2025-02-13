pub trait CameraSynk {
    fn connect(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn send_packet(
        &self,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}
