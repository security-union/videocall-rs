use criterion::{criterion_group, criterion_main, Criterion};
use videocall_bench::{capnp_bench, flatbuf_bench, payloads, proto_bench};

fn bench_health_packet(c: &mut Criterion) {
    let payload = payloads::health_packet_5_peers();

    let proto_msg = proto_bench::build_health_packet(&payload);
    let proto_bytes = proto_bench::encode_health_packet(&proto_msg);
    let capnp_bytes = capnp_bench::encode_health_packet(&payload);
    let fb_bytes = flatbuf_bench::encode_health_packet(&payload);

    println!("\n=== Wire Size: HealthPacket (5 peers) ===");
    println!("  protobuf:    {:>6} bytes", proto_bytes.len());
    println!("  capnproto:   {:>6} bytes", capnp_bytes.len());
    println!("  flatbuffers: {:>6} bytes", fb_bytes.len());

    let mut group = c.benchmark_group("health_packet_5_peers");

    // Encode
    group.bench_function("protobuf/encode", |b| {
        b.iter(|| proto_bench::encode_health_packet(&proto_msg))
    });
    group.bench_function("capnproto/encode", |b| {
        b.iter(|| capnp_bench::encode_health_packet(&payload))
    });
    group.bench_function("flatbuffers/encode", |b| {
        b.iter(|| flatbuf_bench::encode_health_packet(&payload))
    });

    // Decode
    group.bench_function("protobuf/decode", |b| {
        b.iter(|| proto_bench::decode_health_packet(&proto_bytes))
    });
    group.bench_function("capnproto/decode", |b| {
        b.iter(|| capnp_bench::decode_health_packet(&capnp_bytes))
    });
    group.bench_function("flatbuffers/decode", |b| {
        b.iter(|| flatbuf_bench::decode_health_packet(&fb_bytes))
    });

    // Round-trip
    group.bench_function("protobuf/roundtrip", |b| {
        b.iter(|| {
            let bytes = proto_bench::encode_health_packet(&proto_msg);
            proto_bench::decode_health_packet(&bytes)
        })
    });
    group.bench_function("capnproto/roundtrip", |b| {
        b.iter(|| {
            let bytes = capnp_bench::encode_health_packet(&payload);
            capnp_bench::decode_health_packet(&bytes)
        })
    });
    group.bench_function("flatbuffers/roundtrip", |b| {
        b.iter(|| {
            let bytes = flatbuf_bench::encode_health_packet(&payload);
            flatbuf_bench::decode_health_packet(&bytes)
        })
    });

    group.finish();
}

criterion_group!(benches, bench_health_packet);
criterion_main!(benches);
