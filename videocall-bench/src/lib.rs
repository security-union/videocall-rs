// Cap'n Proto generated code — must be at crate root because generated code
// uses `crate::media_packet_capnp::*` paths for cross-type references.
#[allow(clippy::all)]
#[allow(dead_code)]
pub mod media_packet_capnp {
    include!(concat!(env!("OUT_DIR"), "/media_packet_capnp.rs"));
}
#[allow(clippy::all)]
#[allow(dead_code)]
pub mod packet_wrapper_capnp {
    include!(concat!(env!("OUT_DIR"), "/packet_wrapper_capnp.rs"));
}
#[allow(clippy::all)]
#[allow(dead_code)]
pub mod health_packet_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_packet_capnp.rs"));
}

// FlatBuffers generated code — each file defines `mod videocall { mod bench { ... } }`.
// We include them in separate submodules to avoid name collisions.
#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod fb_media {
    include!(concat!(env!("OUT_DIR"), "/media_packet_generated.rs"));
}
#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod fb_packet_wrapper {
    include!(concat!(env!("OUT_DIR"), "/packet_wrapper_generated.rs"));
}
#[allow(clippy::all)]
#[allow(dead_code)]
#[allow(unused_imports)]
pub mod fb_health {
    include!(concat!(env!("OUT_DIR"), "/health_packet_generated.rs"));
}

pub mod capnp_bench;
pub mod flatbuf_bench;
pub mod payloads;
pub mod proto_bench;
