use std::process::Command;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // --- Cap'n Proto codegen ---
    capnpc::CompilerCommand::new()
        .src_prefix("schemas/capnp")
        .file("schemas/capnp/media_packet.capnp")
        .file("schemas/capnp/packet_wrapper.capnp")
        .file("schemas/capnp/health_packet.capnp")
        .output_path(&out_dir)
        .run()
        .expect("capnp codegen failed — install with: brew install capnp");

    // --- FlatBuffers codegen ---
    for schema in &[
        "schemas/flatbuffers/media_packet.fbs",
        "schemas/flatbuffers/packet_wrapper.fbs",
        "schemas/flatbuffers/health_packet.fbs",
    ] {
        let status = Command::new("flatc")
            .args(["--rust", "-o", &out_dir, schema])
            .status()
            .expect("flatc not found — install with: brew install flatbuffers");
        assert!(status.success(), "flatc failed for {}", schema);
    }

    println!("cargo:rerun-if-changed=schemas/");
}
