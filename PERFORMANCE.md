# videocall.rs Performance Guide

This document provides detailed performance metrics, hardware recommendations, and optimization strategies for videocall.rs. All benchmarks and recommendations are based on internal testing and real-world deployments.

## Table of Contents

- [Performance Overview](#performance-overview)
- [Hardware Requirements](#hardware-requirements)
- [Network Requirements](#network-requirements)
- [Scaling Metrics](#scaling-metrics)
- [Call Type Performance](#call-type-performance)
- [Optimization Strategies](#optimization-strategies)
- [Benchmarking Methodology](#benchmarking-methodology)
- [Known Limitations](#known-limitations)

## Performance Overview

videocall.rs is designed for high efficiency across various deployment scenarios, from embedded devices to enterprise-grade servers. Our Rust implementation prioritizes:

- Low CPU and memory utilization
- Minimal network overhead
- Adaptive quality based on available resources
- Efficient media handling with WebCodecs

## Hardware Requirements

### Server-Side Requirements

| Deployment Size | CPU | RAM | Network | Storage |
|-----------------|-----|-----|---------|---------|
| Small (<50 concurrent users) | 2 cores @ 2.5GHz+ | 4GB | 100Mbps | 20GB SSD |
| Medium (50-200 concurrent users) | 4 cores @ 3.0GHz+ | 8GB | 500Mbps | 40GB SSD |
| Large (200-500 concurrent users) | 8 cores @ 3.0GHz+ | 16GB | 1Gbps | 80GB SSD |
| Very Large (500-1000 concurrent users) | 16+ cores @ 3.0GHz+ | 32GB+ | 2+Gbps | 160GB+ SSD |

### Client-Side Requirements

| Device Type | Minimum | Recommended |
|-------------|---------|-------------|
| Desktop Browser | Dual-core CPU @ 2.0GHz, 4GB RAM | Quad-core CPU @ 2.5GHz+, 8GB+ RAM |
| Mobile Browser | Modern smartphone (2019+) | Flagship smartphone (2021+) |
| CLI Client | Raspberry Pi 3+ | Raspberry Pi 4 with 4GB+ RAM |

## Network Requirements

### Bandwidth Per Participant

| Quality | Resolution | FPS | Outbound Bandwidth | Inbound Bandwidth (1 Peer) | Inbound Bandwidth (5 Peers) |
|---------|------------|-----|--------------------|-----------------------------|------------------------------|
| Low | 320x240 | 15 | 100-200 Kbps | 100-200 Kbps | 500-1000 Kbps |
| Medium | 640x480 | 30 | 300-500 Kbps | 300-500 Kbps | 1.5-2.5 Mbps |
| High | 1280x720 | 30 | 800-1200 Kbps | 800-1200 Kbps | 4-6 Mbps |
| Very High | 1920x1080 | 30 | 1.5-2.5 Mbps | 1.5-2.5 Mbps | 7.5-12.5 Mbps |

### Latency Requirements

- **Optimal Experience**: <100ms end-to-end latency
- **Good Experience**: 100-200ms end-to-end latency
- **Acceptable Experience**: 200-300ms end-to-end latency
- **Poor Experience**: >300ms end-to-end latency

## Scaling Metrics

### Single Server Capacity

| Server Spec | Max Concurrent 1:1 Calls | Max Small Group Calls (5 users) | Max Large Meeting (SFU mode) |
|-------------|--------------------------|--------------------------------|------------------------------|
| 2 cores, 4GB RAM | 25 | 10 | 100 viewers, 5 broadcasters |
| 4 cores, 8GB RAM | 50 | 20 | 250 viewers, 10 broadcasters |
| 8 cores, 16GB RAM | 100 | 40 | 500 viewers, 20 broadcasters |
| 16 cores, 32GB RAM | 200 | 80 | 1000 viewers, 30 broadcasters |

### Horizontal Scaling (with NATS)

Using our pub/sub architecture with NATS, videocall.rs scales horizontally with near-linear performance improvements. A cluster of 4 medium-sized servers can handle approximately 3.5x the load of a single server.

## Call Type Performance

### 1-on-1 Calls

- **CPU Usage**: 5-15% per core on modern processors
- **Memory Usage**: 50-100MB per connection
- **Latency**: <100ms on typical broadband connections
- **Packet Loss Resilience**: Maintains quality with up to 5% packet loss

### Small Group Calls (3-10 participants)

- **Topology**: Mesh (direct peer-to-peer)
- **CPU Usage**: Increases approximately linearly with participant count
- **Memory Usage**: 80-150MB per connection
- **Automatic Quality Adjustment**: Reduces resolution when CPU exceeds 70% utilization
- **Recommended Bandwidth**: 2Mbps upload minimum for 4-person HD call

### Large Group Calls (10+ participants)

- **Topology**: Selective Forwarding Unit (SFU)
- **Active Speakers**: Dynamically manages 5-10 active video streams
- **Server CPU Load**: Approximately 0.5 core per active broadcaster
- **Client CPU Load**: Similar to a 5-person call regardless of total participant count
- **Scaling Limit**: Tested with 1000 participants (30 active broadcasters, 970 viewers)

## Optimization Strategies

### Server-Side Optimizations

1. **NATS Configuration**:
   - Increase maximum payload size to 8MB for optimal performance
   - Configure with `--max_payload=8388608`

2. **System Tuning**:
   ```bash
   # Increase max open files
   ulimit -n 65535
   
   # Optimize network settings
   sysctl -w net.core.somaxconn=4096
   sysctl -w net.ipv4.tcp_max_syn_backlog=4096
   sysctl -w net.ipv4.ip_local_port_range="1024 65535"
   ```

3. **Process Priority**:
   - Run with `nice -n -10` for higher priority processing on busy systems

### Client-Side Optimizations

1. **Browser Settings**:
   - Enable hardware acceleration
   - Use Chrome/Chromium for best WebCodecs performance

2. **CLI Client**:
   - Set appropriate `--bitrate-kbps` based on network conditions
   - Use `--frame-format NV12` for optimal encoding performance
   - Adjust `--fps` to match camera capabilities

## Benchmarking Methodology

Our performance metrics are gathered using the following methodology:

1. **Test Environment**:
   - Dedicated bare-metal servers (not virtualized)
   - Controlled network conditions with simulated packet loss and latency
   - Automated test clients generating realistic media traffic

2. **Metrics Collected**:
   - CPU utilization (user, system, wait)
   - Memory consumption
   - Network throughput
   - Media quality metrics (PSNR, VMAF)
   - End-to-end latency

3. **Stress Testing**:
   - Gradual ramp-up of concurrent users until degradation
   - Sustained load tests (24+ hours)
   - Fault injection to verify resilience

## Known Limitations

1. **Browser Compatibility**:
   - Full feature set currently available only on Chrome/Chromium
   - Safari has reduced performance due to limited WebCodecs implementation

2. **Network Conditions**:
   - Performance degrades significantly with >10% packet loss
   - Requires minimum 500Kbps stable bandwidth for basic functionality

3. **Scaling Considerations**:
   - Database becomes bottleneck above 5000 concurrent users
   - NATS cluster requires careful tuning above 2000 concurrent users

4. **Platform-Specific Issues**:
   - ARM processors have approximately 20% lower throughput than x86_64
   - Windows performance is approximately 10% lower than Linux with identical hardware

---

*This performance guide is continually updated based on testing and user feedback. All metrics represent best-case scenarios with properly configured systems.* 