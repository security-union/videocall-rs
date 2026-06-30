use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use videocall_bench::{capnp_bench, flatbuf_bench, payloads, proto_bench};

fn bench_video_5kb(c: &mut Criterion) {
    let payload = payloads::video_media_packet();

    // Pre-build protobuf message and pre-encode all formats for decode benchmarks
    let proto_msg = proto_bench::build_media_packet(&payload);
    let proto_bytes = proto_bench::encode_media_packet(&proto_msg);
    let capnp_bytes = capnp_bench::encode_media_packet(&payload);
    let fb_bytes = flatbuf_bench::encode_media_packet(&payload);

    println!("\n=== Wire Size: MediaPacket (video ~5KB) ===");
    println!("  protobuf:    {:>6} bytes", proto_bytes.len());
    println!("  capnproto:   {:>6} bytes", capnp_bytes.len());
    println!("  flatbuffers: {:>6} bytes", fb_bytes.len());

    let mut group = c.benchmark_group("media_packet_video_5kb");
    group.throughput(Throughput::Bytes(payload.data.len() as u64));

    // Encode
    group.bench_function("protobuf/encode", |b| {
        b.iter(|| proto_bench::encode_media_packet(&proto_msg))
    });
    group.bench_function("capnproto/encode", |b| {
        b.iter(|| capnp_bench::encode_media_packet(&payload))
    });
    group.bench_function("flatbuffers/encode", |b| {
        b.iter(|| flatbuf_bench::encode_media_packet(&payload))
    });

    // Decode
    group.bench_function("protobuf/decode", |b| {
        b.iter(|| proto_bench::decode_media_packet(&proto_bytes))
    });
    group.bench_function("capnproto/decode", |b| {
        b.iter(|| capnp_bench::decode_media_packet(&capnp_bytes))
    });
    group.bench_function("flatbuffers/decode", |b| {
        b.iter(|| flatbuf_bench::decode_media_packet(&fb_bytes))
    });
    group.bench_function("flatbuffers/decode_unchecked", |b| {
        b.iter(|| flatbuf_bench::decode_media_packet_unchecked(&fb_bytes))
    });

    // Round-trip
    group.bench_function("protobuf/roundtrip", |b| {
        b.iter(|| {
            let bytes = proto_bench::encode_media_packet(&proto_msg);
            proto_bench::decode_media_packet(&bytes)
        })
    });
    group.bench_function("capnproto/roundtrip", |b| {
        b.iter(|| {
            let bytes = capnp_bench::encode_media_packet(&payload);
            capnp_bench::decode_media_packet(&bytes)
        })
    });
    group.bench_function("flatbuffers/roundtrip", |b| {
        b.iter(|| {
            let bytes = flatbuf_bench::encode_media_packet(&payload);
            flatbuf_bench::decode_media_packet(&bytes)
        })
    });

    group.finish();
}

fn bench_audio_160b(c: &mut Criterion) {
    let payload = payloads::audio_media_packet();

    let proto_msg = proto_bench::build_media_packet(&payload);
    let proto_bytes = proto_bench::encode_media_packet(&proto_msg);
    let capnp_bytes = capnp_bench::encode_media_packet(&payload);
    let fb_bytes = flatbuf_bench::encode_media_packet(&payload);

    println!("\n=== Wire Size: MediaPacket (audio ~160B) ===");
    println!("  protobuf:    {:>6} bytes", proto_bytes.len());
    println!("  capnproto:   {:>6} bytes", capnp_bytes.len());
    println!("  flatbuffers: {:>6} bytes", fb_bytes.len());

    let mut group = c.benchmark_group("media_packet_audio_160b");
    group.throughput(Throughput::Bytes(payload.data.len() as u64));

    // Encode
    group.bench_function("protobuf/encode", |b| {
        b.iter(|| proto_bench::encode_media_packet(&proto_msg))
    });
    group.bench_function("capnproto/encode", |b| {
        b.iter(|| capnp_bench::encode_media_packet(&payload))
    });
    group.bench_function("flatbuffers/encode", |b| {
        b.iter(|| flatbuf_bench::encode_media_packet(&payload))
    });

    // Decode
    group.bench_function("protobuf/decode", |b| {
        b.iter(|| proto_bench::decode_media_packet(&proto_bytes))
    });
    group.bench_function("capnproto/decode", |b| {
        b.iter(|| capnp_bench::decode_media_packet(&capnp_bytes))
    });
    group.bench_function("flatbuffers/decode", |b| {
        b.iter(|| flatbuf_bench::decode_media_packet(&fb_bytes))
    });
    group.bench_function("flatbuffers/decode_unchecked", |b| {
        b.iter(|| flatbuf_bench::decode_media_packet_unchecked(&fb_bytes))
    });

    // Round-trip
    group.bench_function("protobuf/roundtrip", |b| {
        b.iter(|| {
            let bytes = proto_bench::encode_media_packet(&proto_msg);
            proto_bench::decode_media_packet(&bytes)
        })
    });
    group.bench_function("capnproto/roundtrip", |b| {
        b.iter(|| {
            let bytes = capnp_bench::encode_media_packet(&payload);
            capnp_bench::decode_media_packet(&bytes)
        })
    });
    group.bench_function("flatbuffers/roundtrip", |b| {
        b.iter(|| {
            let bytes = flatbuf_bench::encode_media_packet(&payload);
            flatbuf_bench::decode_media_packet(&bytes)
        })
    });

    group.finish();
}

criterion_group!(benches, bench_video_5kb, bench_audio_160b);
criterion_main!(benches);
