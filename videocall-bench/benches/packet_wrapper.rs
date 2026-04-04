use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use videocall_bench::{capnp_bench, flatbuf_bench, payloads, proto_bench};

fn bench_packet_wrapper(c: &mut Criterion) {
    // Create inner video MediaPacket payload and serialize it with each format.
    // This tests the real-world double-serialization pattern where
    // PacketWrapper.data contains a serialized inner message.
    let inner_payload = payloads::video_media_packet();

    // For protobuf: inner serialized as protobuf (matches current codebase)
    let inner_proto_msg = proto_bench::build_media_packet(&inner_payload);
    let inner_proto_bytes = proto_bench::encode_media_packet(&inner_proto_msg);

    // For capnproto: inner serialized as capnproto
    let inner_capnp_bytes = capnp_bench::encode_media_packet(&inner_payload);

    // For flatbuffers: inner serialized as flatbuffers
    let inner_fb_bytes = flatbuf_bench::encode_media_packet(&inner_payload);

    // Build wrapper payloads with format-specific inner bytes
    let proto_wrapper_payload = payloads::video_packet_wrapper(inner_proto_bytes.clone());
    let capnp_wrapper_payload = payloads::video_packet_wrapper(inner_capnp_bytes.clone());
    let fb_wrapper_payload = payloads::video_packet_wrapper(inner_fb_bytes.clone());

    // Pre-encode wrappers for decode benchmarks
    let proto_wrapper_msg = proto_bench::build_packet_wrapper(&proto_wrapper_payload);
    let proto_bytes = proto_bench::encode_packet_wrapper(&proto_wrapper_msg);
    let capnp_bytes = capnp_bench::encode_packet_wrapper(&capnp_wrapper_payload);
    let fb_bytes = flatbuf_bench::encode_packet_wrapper(&fb_wrapper_payload);

    println!("\n=== Wire Size: PacketWrapper (wrapping video MediaPacket) ===");
    println!("  protobuf:    {:>6} bytes", proto_bytes.len());
    println!("  capnproto:   {:>6} bytes", capnp_bytes.len());
    println!("  flatbuffers: {:>6} bytes", fb_bytes.len());

    let data_len = inner_proto_bytes.len() as u64;
    let mut group = c.benchmark_group("packet_wrapper_video");
    group.throughput(Throughput::Bytes(data_len));

    // Encode
    group.bench_function("protobuf/encode", |b| {
        b.iter(|| proto_bench::encode_packet_wrapper(&proto_wrapper_msg))
    });
    group.bench_function("capnproto/encode", |b| {
        b.iter(|| capnp_bench::encode_packet_wrapper(&capnp_wrapper_payload))
    });
    group.bench_function("flatbuffers/encode", |b| {
        b.iter(|| flatbuf_bench::encode_packet_wrapper(&fb_wrapper_payload))
    });

    // Decode
    group.bench_function("protobuf/decode", |b| {
        b.iter(|| proto_bench::decode_packet_wrapper(&proto_bytes))
    });
    group.bench_function("capnproto/decode", |b| {
        b.iter(|| capnp_bench::decode_packet_wrapper(&capnp_bytes))
    });
    group.bench_function("flatbuffers/decode", |b| {
        b.iter(|| flatbuf_bench::decode_packet_wrapper(&fb_bytes))
    });

    // Round-trip
    group.bench_function("protobuf/roundtrip", |b| {
        b.iter(|| {
            let bytes = proto_bench::encode_packet_wrapper(&proto_wrapper_msg);
            proto_bench::decode_packet_wrapper(&bytes)
        })
    });
    group.bench_function("capnproto/roundtrip", |b| {
        b.iter(|| {
            let bytes = capnp_bench::encode_packet_wrapper(&capnp_wrapper_payload);
            capnp_bench::decode_packet_wrapper(&bytes)
        })
    });
    group.bench_function("flatbuffers/roundtrip", |b| {
        b.iter(|| {
            let bytes = flatbuf_bench::encode_packet_wrapper(&fb_wrapper_payload);
            flatbuf_bench::decode_packet_wrapper(&bytes)
        })
    });

    group.finish();
}

criterion_group!(benches, bench_packet_wrapper);
criterion_main!(benches);
