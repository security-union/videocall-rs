# videocall-flatbuffers

FlatBuffer message types for the videocall streaming platform.

This crate contains the FlatBuffer schema definitions and generated Rust code for all protocol messages used in the videocall system.

## Building

The FlatBuffer schemas are compiled using the `flatc` compiler. To regenerate the Rust code:

```bash
make generate
```

## Schemas

All `.fbs` schema files are located in the `schemas/` directory.
