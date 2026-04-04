# Transport Serialization Research: Protobuf vs Cap'n Proto vs FlatBuffers

## Context

`videocall-rs` currently uses **Protocol Buffers (rust-protobuf 3.7.1)** for all wire serialization across 9 `.proto` files defining ~20 message types. The serialization layer is used on both the server (actix-api) and client (videocall-client targeting `wasm32-unknown-unknown`). This document evaluates **Cap'n Proto** and **FlatBuffers** as potential replacements.

---

## Current Protobuf Usage Summary

| Aspect | Detail |
|--------|--------|
| Proto files | 9 files in `protobuf/types/` |
| Key messages | `PacketWrapper`, `MediaPacket`, `HealthPacket`, `DiagnosticsPacket`, `AesPacket`, `RsaPacket`, `ConnectionPacket`, `MeetingPacket`, `ServerConnectionPacket` |
| Rust crate | `protobuf = "3.7.1"` (also `prost = "0.11"` in some crates) |
| Code generation | Docker-based `protoc --rs_out` via Makefile |
| Serialization calls | `write_to_bytes()` / `parse_from_bytes()` in ~15+ source files |
| Targets | Native (server) + `wasm32-unknown-unknown` (client) |
| Wire pattern | Nested: `PacketWrapper.data` contains serialized inner messages |

---

## Candidate Comparison

### 1. Serialization Model

| Feature | Protocol Buffers | Cap'n Proto | FlatBuffers |
|---------|-----------------|-------------|-------------|
| **Encoding model** | Encode/decode to/from bytes | Zero-copy — wire format IS the in-memory format | Zero-copy read, builder-based write |
| **Random access** | No (must deserialize entire message) | Yes (pointer-based, relative pointers) | Yes (offset-table-based) |
| **Schema evolution** | Excellent (field numbers) | Good (ordinal-based) | Good (vtable-based) |
| **Nested messages** | Serialize inner, embed as `bytes` field | Native nesting, zero-copy traversal | Native nesting, zero-copy traversal |

**Implications for videocall-rs:** The current `PacketWrapper.data = inner.write_to_bytes()` pattern requires double serialization. Both Cap'n Proto and FlatBuffers support native nesting, which would eliminate this overhead.

### 2. Performance Benchmarks

#### Rust Benchmarks ([kcchu/buffer-benchmarks](https://github.com/kcchu/buffer-benchmarks))

| Metric | rust-protobuf | prost | FlatBuffers |
|--------|---------------|-------|-------------|
| **Encode** | 705 ns/op | 643 ns/op | 878 ns/op |
| **Decode** | 752 ns/op | 1059 ns/op | **331 ns/op** |
| **Wire size** | 299 bytes | 299 bytes | 428 bytes |

#### C++ Benchmarks ([CppSerialization](https://github.com/chronoxor/CppSerialization), Apple M1 Pro)

| Metric | Protobuf | Cap'n Proto | FlatBuffers |
|--------|----------|-------------|-------------|
| **Serialize** | 322 ns | 247 ns | 272 ns |
| **Deserialize** | 351 ns | **184 ns** | **81 ns** |
| **Wire size** | **120 bytes** | 208 bytes | 280 bytes |

#### Key Takeaways

- **Decode speed:** FlatBuffers and Cap'n Proto are **2-4x faster** than Protobuf at deserialization. This is critical for a real-time video call application where every frame goes through decode.
- **Encode speed:** All three are comparable; Cap'n Proto edges ahead slightly.
- **Wire size:** Protobuf is the most compact (**40-60% smaller**). Cap'n Proto and FlatBuffers trade size for speed due to alignment padding.
- **For videocall-rs:** Media packets are the hot path. Faster decode matters more than wire size since the data field already contains large codec payloads (VP8/VP9/Opus frames) that dwarf the metadata overhead.

### 3. WASM / `wasm32-unknown-unknown` Compatibility

| Feature | Cap'n Proto (`capnp`) | FlatBuffers (`flatbuffers`) |
|---------|----------------------|---------------------------|
| **Compiles to wasm32** | Yes — `no_std` + `no_alloc` supported | Yes — minimal std dependency, `core`/`alloc` only |
| **External tooling needed** | `capnpc` binary for code generation (build-time only) | `flatc` binary for code generation (build-time only) |
| **Runtime dependencies** | Pure Rust, no system calls | Pure Rust, no system calls |
| **Known WASM usage** | Used in Cloudflare Workers (WASM environment) | Used in Google game engines, various WASM projects |

Both are viable for the `wasm32-unknown-unknown` target. Code generation happens at build time (native), and only the runtime library needs to compile to WASM.

### 4. Licensing

| Library | License | Notes |
|---------|---------|-------|
| Protocol Buffers | BSD 3-Clause | Google project |
| rust-protobuf | MIT | Community implementation |
| Cap'n Proto (core) | MIT | Created by Kenton Varda (ex-Protobuf v2 author at Google) |
| capnproto-rust | MIT | Maintained by David Renshaw |
| FlatBuffers | Apache 2.0 | Google project |
| flatbuffers (Rust crate) | Apache 2.0 | Part of official FlatBuffers repo |

All licenses are permissive and compatible with `videocall-rs` (MIT/Apache-2.0 dual licensed). No licensing concerns with any option.

### 5. Ecosystem & Maturity

| Factor | Cap'n Proto | FlatBuffers |
|--------|-------------|-------------|
| **Origin** | Kenton Varda (Protobuf v2 author), 2013 | Google, 2014 |
| **Rust crate maturity** | `capnp` v0.20+ — actively maintained, async RPC support | `flatbuffers` v24.x — maintained as part of Google's monorepo |
| **crates.io downloads** | ~3.5M total | ~12M total |
| **Language support** | C++, Rust, Go, Java, JS, Python, others | C++, Rust, Go, Java, JS, C#, Python, Kotlin, Swift, many more |
| **RPC framework** | Built-in Cap'n Proto RPC | None built-in (typically paired with gRPC or custom) |
| **Tooling** | `capnpc` compiler + Rust plugin | `flatc` compiler with built-in Rust codegen |
| **Schema language** | Custom `.capnp` format | Custom `.fbs` format (IDL similar to C-like structs) |
| **Major users** | Cloudflare, Sandstorm | Google (internally), Facebook/Meta, Netflix |

### 6. Developer Experience

| Factor | Cap'n Proto | FlatBuffers |
|--------|-------------|-------------|
| **Generated code style** | Builder/Reader pattern with lifetime-bound borrows | Builder pattern (write) + generated accessor structs (read) |
| **Learning curve** | Steeper — ownership model with readers/builders | Moderate — more straightforward API |
| **Error handling** | Traversal limits + bounds checking built-in | Optional verifier pass; raw access is unchecked by default |
| **Schema syntax** | Unique syntax, requires learning | Familiar C-like IDL |
| **Documentation** | Good, but smaller community | Extensive docs, larger community |
| **Debugging** | Harder (binary format, fewer tools) | Harder (binary format), but `flatc` has JSON conversion |

### 7. Migration Effort

Both would require:
1. **Rewrite 9 schema files** from `.proto` to `.capnp` or `.fbs`
2. **Update code generation** pipeline (Docker-based Makefile)
3. **Update ~15+ source files** with new serialize/deserialize calls
4. **Eliminate double-serialization** pattern (`PacketWrapper.data` containing pre-serialized inner messages)
5. **Update 5 Cargo.toml files** (videocall-types, videocall-client, actix-api, videocall-cli, bot)
6. **Fix version mismatch** (currently protobuf 3.7.1 vs 3.3.0 across crates)

**Cap'n Proto-specific concerns:**
- Builder/Reader pattern requires more refactoring of existing code
- The `.capnp` schema syntax is less familiar
- RPC capabilities are unused (this project uses WebTransport/WebSocket, not Cap'n Proto RPC)

**FlatBuffers-specific concerns:**
- `flatc` must be installed or vendored for code generation
- No built-in verifier by default in Rust (need to call verifier explicitly)
- Slightly larger wire size

---

## Recommendation Matrix

| Priority | Weight | Protobuf (current) | Cap'n Proto | FlatBuffers |
|----------|--------|-------------------|-------------|-------------|
| Decode speed (hot path) | High | Baseline | Excellent | Excellent |
| WASM compatibility | High | Proven | Good | Good |
| Wire size efficiency | Medium | Best | Acceptable | Acceptable |
| Migration effort | Medium | N/A (current) | Higher | Lower |
| Community / ecosystem | Medium | Largest | Smaller | Large |
| Developer ergonomics | Medium | Familiar | Steeper curve | Moderate |
| Schema evolution | Medium | Excellent | Good | Good |
| Zero-copy potential | High | None | Full | Read-only |
| License compatibility | Low | All fine | All fine | All fine |

---

## Summary

### FlatBuffers — Recommended if migrating

- **Best decode performance** in Rust benchmarks (2x faster than protobuf)
- **Larger ecosystem** and more language support than Cap'n Proto
- **Lower migration effort** — more familiar schema syntax, simpler API
- **Google-backed** with long-term maintenance track record
- **Trade-off:** ~43% larger wire size than protobuf, but for a video call app where codec payloads dominate, metadata overhead is negligible

### Cap'n Proto — Strong alternative

- **True zero-copy** (both read and write) — theoretically fastest possible
- **Excellent C++ benchmarks** but fewer Rust-specific benchmarks available
- **Built-in RPC** (unused for this project but future-proof)
- **Trade-off:** Steeper learning curve, smaller community, more complex ownership model in Rust

### Stay with Protobuf — Valid option

- **No migration cost**
- **Smallest wire size**
- **Largest ecosystem**
- **Trade-off:** Slowest decode, no zero-copy, double-serialization pattern for nested messages

---

## Sources

- [kcchu/buffer-benchmarks (Rust/Go)](https://github.com/kcchu/buffer-benchmarks)
- [CppSerialization Benchmarks](https://github.com/chronoxor/CppSerialization)
- [Cap'n Proto Official Site](https://capnproto.org/news/2014-06-17-capnproto-flatbuffers-sbe.html)
- [capnproto-rust GitHub](https://github.com/capnproto/capnproto-rust)
- [FlatBuffers Official Docs](https://flatbuffers.dev/)
- [FlatBuffers Rust Docs](https://flatbuffers.dev/languages/rust/)
- [FlatBuffers Benchmarks](https://flatbuffers.dev/benchmarks/)
- [Cap'n Proto Wikipedia](https://en.wikipedia.org/wiki/Cap'n_Proto)
- [FlatBuffers Wikipedia](https://en.wikipedia.org/wiki/FlatBuffers)
