# videocall-diagnostics

A lightweight diagnostics event bus for the videocall-rs project. This crate provides a unified way to emit and collect diagnostic metrics across all subsystems in the videocall ecosystem.

## ⚠️ Private Crate Notice

**This is a private crate that is part of the videocall-rs project and is subject to change without notice.** The API is not considered stable and may undergo breaking changes between versions. This crate is not intended for external use outside of the videocall-rs ecosystem.

## Features

- **Cross-platform**: Works on both native and WASM32 targets without requiring Tokio
- **Lightweight**: Minimal dependencies and overhead
- **Multi-producer/multi-consumer**: Uses flume channels for efficient event distribution
- **Structured metrics**: Type-safe metric values (i64, u64, f64, String)
- **Global event bus**: Easy-to-use global sender/receiver system
- **Timestamped events**: Automatic timestamp generation for both native and web environments

## Usage in videocall-rs

This crate is used throughout the videocall-rs project to provide observability and debugging capabilities:

- **videocall-client**: Core client library emits connection, codec, and transport metrics
- **yew-ui**: Web frontend subscribes to diagnostics for real-time monitoring and debugging
- **Other subsystems**: Any component can emit diagnostic events for centralized collection

### Basic Usage

```rust
use videocall_diagnostics::{DiagEvent, global_sender, subscribe, now_ms, metric};

// Emit a diagnostic event
let sender = global_sender();
let event = DiagEvent {
    subsystem: "neteq",
    stream_id: Some("peer_123".to_string()),
    ts_ms: now_ms(),
    metrics: vec![
        metric!("buffer_size", 1024_u64),
        metric!("latency_ms", 45.5_f64),
        metric!("codec", "opus"),
    ],
};
sender.send(event).ok();

// Subscribe to all diagnostic events
let receiver = subscribe();
while let Ok(event) = receiver.recv() {
    println!("Received diagnostic: {:?}", event);
}
```

## Architecture

The crate provides a simple global broadcast system where:

1. **Producers** use `global_sender()` to emit `DiagEvent` instances
2. **Consumers** use `subscribe()` to receive all future events
3. **Events** contain subsystem identification, optional stream IDs, timestamps, and structured metrics

This allows for flexible monitoring and debugging without tight coupling between components.

## Dependencies

- `serde` - Serialization support for diagnostic events
- `flume` - Multi-producer multi-consumer channels
- `js-sys` - WASM time utilities (WASM32 targets only)

---

*Part of the videocall-rs project by Security Union LLC*
