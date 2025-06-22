# Rust NetEQ - Adaptive Jitter Buffer for Audio

A Rust implementation of a NetEQ-inspired adaptive jitter buffer for real-time audio applications. This library provides robust handling of network jitter, packet loss, and timing variations in audio streaming applications.

## Features

### Core Jitter Buffer Functionality
- **Adaptive Delay Management**: Automatically adjusts buffer size based on network conditions
- **Packet Reordering**: Handles out-of-order packet arrival
- **Time-Stretching**: Accelerate and preemptive expand algorithms for buffer management
- **Loss Concealment**: Audio concealment during packet loss events
- **Smart Buffer Management**: Intelligent flushing and overflow handling

### Advanced Capabilities
- **Comprehensive Statistics**: Detailed metrics for network analysis and debugging
- **Configurable Parameters**: Tunable settings for different network conditions
- **Low Latency**: Optimized for real-time audio applications
- **Memory Efficient**: Minimal memory footprint with configurable limits

## Architecture

The implementation follows libWebRTC's NetEQ design with the following key components:

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│   AudioPacket   │    │  PacketBuffer   │    │  DelayManager   │
│                 │───▶│                 │───▶│                 │
│ • RTP Header    │    │ • Ordered Queue │    │ • Adaptive      │
│ • Audio Data    │    │ • Duplicate Det │    │ • Statistics    │
│ • Timestamps    │    │ • Smart Flush   │    │ • Target Delay  │
└─────────────────┘    └─────────────────┘    └─────────────────┘
                                   │
                                   ▼
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│ TimeStretching  │    │     NetEQ       │    │   Statistics    │
│                 │◀───│                 │───▶│                 │
│ • Accelerate    │    │ • Decision      │    │ • Network Stats │
│ • Preemptive    │    │ • Decode        │    │ • Lifetime      │
│ • Expand        │    │ • Control       │    │ • Operations    │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

## Quick Start

Add this to your `Cargo.toml`:

```toml
[dependencies]
rust_neteq = "0.1.0"
log = "0.4"
```

### Basic Usage

```rust
use rust_neteq::{NetEq, NetEqConfig, AudioPacket, RtpHeader};

// Create configuration
let mut config = NetEqConfig::default();
config.sample_rate = 16000;
config.channels = 1;
config.min_delay_ms = 20;
config.max_delay_ms = 500;

// Create NetEQ instance
let mut neteq = NetEq::new(config)?;

// Create and insert audio packet
let header = RtpHeader::new(seq_num, timestamp, ssrc, payload_type, false);
let packet = AudioPacket::new(header, audio_data, 16000, 1, 20);
neteq.insert_packet(packet)?;

// Retrieve audio frame (10ms)
let frame = neteq.get_audio()?;
println!("Retrieved {} samples", frame.samples.len());

// Get statistics
let stats = neteq.get_statistics();
println!("Buffer size: {}ms", stats.current_buffer_size_ms);
println!("Target delay: {}ms", stats.target_delay_ms);
```

### Configuration Options

```rust
let mut config = NetEqConfig {
    sample_rate: 16000,              // Audio sample rate
    channels: 1,                     // Number of channels
    max_packets_in_buffer: 200,      // Maximum buffer capacity
    min_delay_ms: 20,                // Minimum target delay
    max_delay_ms: 500,               // Maximum allowed delay
    enable_fast_accelerate: true,    // Enable aggressive acceleration
    enable_muted_state: false,       // Muted state detection
    for_test_no_time_stretching: false, // Disable time stretching
    // ... additional configuration options
};
```

## Core Components

### 1. Packet Buffer (`PacketBuffer`)

Manages incoming audio packets with intelligent buffering:

- **Ordered Storage**: Maintains packets in timestamp order
- **Duplicate Detection**: Prevents duplicate packet processing
- **Smart Flushing**: Adaptive buffer management during overflows
- **Age Management**: Automatic cleanup of stale packets

```rust
let mut buffer = PacketBuffer::new(100);
buffer.insert_packet(packet, &mut stats, target_delay)?;
let next_packet = buffer.get_next_packet();
```

### 2. Delay Manager (`DelayManager`)

Adaptively controls jitter buffer delay:

- **Quantile-based Estimation**: Uses configurable quantiles for delay calculation
- **Exponential Smoothing**: Gradual adaptation to changing conditions
- **Constraint Handling**: Respects minimum/maximum delay limits

```rust
let mut delay_manager = DelayManager::new(delay_config);
delay_manager.update(timestamp, sample_rate, false)?;
let target_delay = delay_manager.target_delay_ms();
```

### 3. Time Stretching (`TimeStretcher`)

Implements audio time modification algorithms:

#### Accelerate Algorithm
- Removes audio samples to speed up playback
- Energy-based removal point selection
- Crossfading to minimize artifacts

#### Preemptive Expand Algorithm
- Adds audio samples to slow down playback
- Correlation-based duplication point selection
- Overlap-add synthesis for smooth transitions

```rust
let accelerate = TimeStretchFactory::create_accelerate(16000, 1);
let mut output = Vec::new();
let result = accelerate.process(&input, &mut output, false);
```

### 4. Statistics (`StatisticsCalculator`)

Comprehensive metrics collection similar to libWebRTC:

```rust
let stats = neteq.get_statistics();

// Network statistics
println!("Current buffer: {}ms", stats.network.current_buffer_size_ms);
println!("Mean waiting time: {}ms", stats.network.mean_waiting_time_ms);
println!("Accelerate rate: {}", stats.network.accelerate_rate);

// Lifetime statistics  
println!("Packets received: {}", stats.lifetime.jitter_buffer_packets_received);
println!("Concealment events: {}", stats.lifetime.concealment_events);
println!("Buffer flushes: {}", stats.lifetime.buffer_flushes);
```

## Algorithm Details

### Adaptive Delay Control

The delay manager uses a quantile-based approach to estimate the optimal buffer delay:

1. **Arrival Tracking**: Monitors inter-arrival times of packets
2. **Quantile Estimation**: Calculates delay based on configurable quantile (default 95th percentile)
3. **Exponential Smoothing**: Applies smoothing with configurable forget factor
4. **Constraint Application**: Enforces minimum/maximum delay bounds

### Decision Logic

NetEQ makes frame-by-frame decisions on what operation to perform:

```rust
if buffer_empty {
    Operation::Expand           // Concealment
} else if buffer_too_full {
    Operation::Accelerate       // Time compression
} else if buffer_getting_low {
    Operation::PreemptiveExpand // Time expansion  
} else {
    Operation::Normal           // Standard decode
}
```

### Time-Stretching Quality

Both accelerate and preemptive expand algorithms include:

- **Energy Analysis**: Identifies low-energy regions for processing
- **Correlation Detection**: Finds suitable modification points
- **Artifact Minimization**: Uses crossfading and overlap-add techniques

## Performance Characteristics

### Memory Usage
- **Packet Storage**: ~1-5MB typical usage (depends on buffer size)
- **Processing Overhead**: Minimal per-frame allocation
- **Statistics**: ~1KB for comprehensive metrics

### Latency
- **Processing Delay**: <1ms per 10ms frame
- **Buffer Delay**: Adaptive (typically 20-100ms)
- **Algorithm Delay**: <5ms for time-stretching operations

### CPU Usage
- **Normal Operation**: Very low CPU usage
- **Time-Stretching**: Moderate CPU increase during adaptation
- **Statistics**: Negligible overhead

## Testing

Run the test suite:

```bash
cargo test
```

Run with logging:

```bash
RUST_LOG=debug cargo test
```

Run the example:

```bash
cargo run --example basic_usage
```

## Examples

### Basic Jitter Buffer

```rust
use rust_neteq::{NetEq, NetEqConfig};

let config = NetEqConfig::default();
let mut neteq = NetEq::new(config)?;

// Process packets and retrieve audio...
```

### Custom Configuration

```rust
let mut config = NetEqConfig::default();
config.sample_rate = 48000;
config.channels = 2;
config.min_delay_ms = 30;
config.enable_fast_accelerate = true;

let mut neteq = NetEq::new(config)?;
```

### Statistics Monitoring

```rust
let stats = neteq.get_statistics();
log::info!("Buffer utilization: {:.1}%", 
    stats.current_buffer_size_ms as f32 / stats.target_delay_ms as f32 * 100.0);
```

## Integration Notes

### Audio Codecs
This library handles decoded audio (f32 samples). It should be used after audio codec decoding:

```
Network → RTP → Codec Decode → NetEQ → Audio Output
```

### Real-time Usage
For real-time applications:

1. Call `get_audio()` every 10ms
2. Insert packets as they arrive
3. Monitor statistics for network analysis
4. Adjust configuration based on network conditions

### Threading
NetEQ is not thread-safe. Use appropriate synchronization for multi-threaded applications.

## Comparison to libWebRTC NetEQ

| Feature | Rust NetEQ | libWebRTC NetEQ |
|---------|------------|-----------------|
| Adaptive Delay | ✅ | ✅ |
| Time Stretching | ✅ (Basic) | ✅ (Advanced) |
| Packet Reordering | ✅ | ✅ |
| Loss Concealment | ✅ (Basic) | ✅ (Advanced) |
| Statistics | ✅ | ✅ |
| DTMF Support | ❌ | ✅ |
| Multiple Codecs | ❌ | ✅ |
| Voice Detection | ❌ | ✅ |

## Contributing

Contributions are welcome! Please see the [contributing guidelines](CONTRIBUTING.md).

### Areas for Enhancement

1. **Advanced Concealment**: More sophisticated loss concealment algorithms
2. **Voice Activity Detection**: VAD integration for better quality
3. **Multi-codec Support**: Support for different audio codecs
4. **DTMF Detection**: Dual-tone multi-frequency support
5. **Performance Optimization**: Further CPU and memory optimizations

## License

Licensed under MIT license ([LICENSE](LICENSE) or http://opensource.org/licenses/MIT)

## References

- [WebRTC NetEQ Documentation](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_coding/neteq/)
- [RFC 3550 - RTP: A Transport Protocol for Real-Time Applications](https://tools.ietf.org/html/rfc3550)
- [Adaptive Jitter Buffer for Internet Telephony](https://citeseerx.ist.psu.edu/viewdoc/summary?doi=10.1.1.91.5396) 