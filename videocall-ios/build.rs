fn main() {
    // Ensure Cargo knows when to rerun the build script
    println!("cargo:rerun-if-changed=src/videocall.udl");
    println!("cargo:rerun-if-changed=Cargo.toml");

    // Generate Rust scaffolding code from UDL
    uniffi_build::generate_scaffolding("src/videocall.udl").unwrap();
}
