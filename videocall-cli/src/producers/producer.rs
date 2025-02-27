use anyhow::Result;

pub trait Producer {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}
