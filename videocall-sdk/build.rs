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

fn main() {
    // Ensure Cargo knows when to rerun the build script
    println!("cargo:rerun-if-changed=src/videocall.udl");
    println!("cargo:rerun-if-changed=Cargo.toml");

    // Generate Rust scaffolding code from UDL
    uniffi_build::generate_scaffolding("src/videocall.udl").unwrap();
}
