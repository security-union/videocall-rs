# videocall-flatbuffers

FlatBuffer message types for the videocall streaming platform.

This crate contains the FlatBuffer schema definitions and generated Rust code for all protocol messages used in the videocall system.

## Building

### Local Build (requires flatc)

The FlatBuffer schemas are compiled using the `flatc` compiler. To regenerate the Rust code:

```bash
make generate
```

To install flatc locally:
```bash
make install-flatc
```

### Docker Build (recommended for CI/production)

Build and generate code using Docker (no local dependencies required):

```bash
# Build the Docker image
make build-env

# Generate Rust code in Docker
make build-env-generate

# Or do both in one command
make docker-build
```

## Schemas

All `.fbs` schema files are located in the `schemas/` directory:

- `packet_wrapper.fbs` - Main packet wrapper with type discrimination
- `media_packet.fbs` - Audio/video/screen media packets
- `aes_packet.fbs` - AES encryption key exchange
- `rsa_packet.fbs` - RSA public key exchange
- `connection_packet.fbs` - Connection initialization
- `diagnostics_packet.fbs` - Real-time diagnostics data
- `health_packet.fbs` - Health monitoring and stats
- `server_connection_packet.fbs` - Server connection events

## Generated Code

Generated Rust code is placed in `src/generated/` and is automatically included in the crate via `src/lib.rs`.

## Development Workflow

1. Edit schema files in `schemas/`
2. Run `make generate` (or `make docker-build` for Docker)
3. Generated code appears in `src/generated/`
4. Run `cargo test` to verify

## CI Integration

The Makefile supports both local and Docker-based builds, making it easy to integrate into CI pipelines:

```yaml
- name: Generate FlatBuffer code
  run: cd videocall-flatbuffers && make docker-build
  
- name: Test
  run: cd videocall-flatbuffers && cargo test
```
