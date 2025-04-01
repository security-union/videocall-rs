// Include the generated bindings
uniffi::include_scaffolding!("videocall");

use crate::videocall::get_version;
use crate::videocall::hello_world;

// Module that implements the UDL interface
pub mod videocall {
    use log::info;

    // A simple function that returns a greeting
    pub fn hello_world() -> String {
        info!("hello_world function called");
        "Hello from Rust!".to_string()
    }

    // Return the version of the library
    pub fn get_version() -> String {
        info!("get_version function called");
        env!("CARGO_PKG_VERSION").to_string()
    }
}
