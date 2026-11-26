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

pub trait CameraSynk {
    fn connect(&mut self) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn send_packet(
        &self,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Resolves, with a reason, once the connection behind this sink is gone
    /// for good. Sinks without a connection never resolve.
    fn lost(&self) -> impl std::future::Future<Output = String> + Send {
        std::future::pending()
    }

    /// Close the connection cleanly, if there is one.
    fn close(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }
}
