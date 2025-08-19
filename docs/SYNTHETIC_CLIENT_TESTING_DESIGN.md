# Synthetic Client Testing Tool Design Document

## Overview

This design document outlines a comprehensive solution for testing videocall-rs at scale using synthetic clients that generate realistic video and audio traffic. The tool will enable load testing, performance validation, and scalability assessment of the videocall infrastructure.

## Executive Summary

**Project Name**: `videocall-synthetic-clients`
**Purpose**: Cloud-based synthetic client generator for videocall-rs load testing
**Target**: Scale testing with configurable synthetic audio/video streams
**Technology Stack**: Rust, Kubernetes, Helm, QUIC/WebTransport

## Requirements Analysis

### Functional Requirements

1. **Multi-Client Support**
   - Deploy N configurable synthetic clients simultaneously
   - Each client operates independently with unique identity
   - Support for 100+ concurrent clients per deployment

2. **Synthetic Media Generation**
   - **Video**: Pre-recorded video file playback or procedurally generated patterns
   - **Audio**: Pre-recorded audio file playback or synthetic audio generation
   - **Configurable**: Enable/disable audio and video per client independently

3. **Meeting Management**
   - Support multiple meeting rooms simultaneously
   - Configurable meeting ID per client or client groups
   - As soon as the client is created, it will join the meeting and start sending media.

4. **Cloud Deployment**
   - Helm chart for Kubernetes deployment
   - Horizontal scaling capabilities
   - Resource optimization for cost-effective testing

### Non-Functional Requirements

1. **Scalability**: Support 1000+ concurrent synthetic clients
2. **Performance**: Minimal overhead per synthetic client (<50MB RAM, <100m CPU)
3. **Maintainability**: Clean architecture with reusable components

## Clarified Requirements (Based on User Input)

### 1. **Protocol: WebTransport Only**
- Replace WebSocket with WebTransport using `web-transport-quinn` 
- Server URL configurable via env var, defaulting to `https://webtransport-us-east.webtransport.video`
- All media packets sent via **unidirectional streams** (not datagrams)

### 2. **Media Protocol & Timing**
- Use existing protobuf `MediaPacket` format
- **Audio**: 50fps (20ms Opus packets) - following `neteq_player.rs` pattern
- **Video**: 30fps (~33ms VP9 packets) - following `videocall-cli` pattern

### 3. **Media Assets (Baked into Docker)**
- **Audio**: `BundyBests2.wav` (already in bot directory) encoded to Opus packets
- **Video**: JPEG sequence from `videocall-cli/assets/images/sample_video_save/`
- **Distribution**: Assets baked into Docker image
- **WebTransport URL**: Same structure as WebSocket: `/lobby/{user_id}/{meeting_id}`

### 4. **Per-Client Configuration (YAML)**
- Each container runs multiple clients with individual configurations
- Configuration via YAML file (works with both Docker volumes and Helm ConfigMaps)
- Linear ramp-up with configurable sleep between client starts
- Example config:
```yaml
ramp_up_delay_ms: 1000  # Sleep between client starts
server_url: "https://webtransport-us-east.webtransport.video"
clients:
  - user_id: "bot001"
    meeting_id: "test-room-1" 
    enable_audio: true
    enable_video: true
  - user_id: "bot002"
    meeting_id: "test-room-2"
    enable_audio: true
    enable_video: false
```

### 5. **Multi-Client Architecture** 
- Multiple synthetic clients per container instance
- Resource optimization through shared encoding infrastructure

## Architecture Design

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes Cluster                      │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐│
│  │  Synthetic      │  │  Synthetic      │  │  Synthetic      ││
│  │  Client Pod 1   │  │  Client Pod 2   │  │  Client Pod N   ││
│  │                 │  │                 │  │                 ││
│  │ ┌─────────────┐ │  │ ┌─────────────┐ │  │ ┌─────────────┐ ││
│  │ │Audio Engine │ │  │ │Audio Engine │ │  │ │Audio Engine │ ││
│  │ │Video Engine │ │  │ │Video Engine │ │  │ │Video Engine │ ││
│  │ │QUIC Client  │ │  │ │QUIC Client  │ │  │ │QUIC Client  │ ││
│  │ │Metrics      │ │  │ │Metrics      │ │  │ │Metrics      │ ││
│  │ └─────────────┘ │  │ └─────────────┘ │  │ └─────────────┘ ││
│  └─────────────────┘  └─────────────────┘  └─────────────────┘│
│                                                               │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐│
│  │   ConfigMap     │  │    Metrics      │  │   Asset Store   ││
│  │ (Client Config) │  │   Collector     │  │ (Media Files)   ││
│  └─────────────────┘  └─────────────────┘  └─────────────────┘│
└─────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────┐
│                    videocall-rs Infrastructure              │
│                (QUIC/WebTransport Servers)                 │
└─────────────────────────────────────────────────────────────┘
```

### Component Architecture

#### 1. Synthetic Client Core (`videocall-synthetic-client`)

**Location**: New crate `videocall-synthetic-client/`

**Responsibilities**:
- QUIC/WebTransport connection management
- Media stream orchestration
- Metrics collection and reporting
- Configuration management

**Key Components**:
```rust
pub struct SyntheticClient {
    config: ClientConfig,
    quic_client: QUICClient,
    audio_engine: Option<AudioEngine>,
    video_engine: Option<VideoEngine>,
    metrics_collector: MetricsCollector,
}

pub struct ClientConfig {
    pub user_id: String,
    pub meeting_id: String,
    pub server_url: Url,
    pub enable_audio: bool,
    pub enable_video: bool,
    pub audio_config: Option<AudioConfig>,
    pub video_config: Option<VideoConfig>,
    pub metrics_config: MetricsConfig,
}
```

#### 2. Audio Engine

**Responsibilities**:
- Synthetic audio generation or file playback
- Opus encoding
- Audio packet timing and transmission

**Implementation Options**:
1. **File Playback**: Loop pre-recorded audio files (WAV/MP3)

```rust
pub trait AudioEngine {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn generate_frame(&mut self) -> Result<Vec<u8>>;
}
```

#### 3. Video Engine

**Responsibilities**:
- Synthetic video generation or file playback
- VP9 encoding
- Video packet timing and transmission

**Implementation Options**:
1. **File Playback**: Loop pre-recorded video sequences

```rust
pub trait VideoEngine {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn generate_frame(&mut self) -> Result<Vec<u8>>;
}

pub struct FileBasedVideoEngine {
    frames: Vec<Vec<u8>>,
    current_frame: usize,
    framerate: u32,
    encoder: VpxEncoder,
}
```

#### 4. Metrics Collection

**Responsibilities**:
- Connection quality metrics
- Performance monitoring
- Resource utilization tracking
- Export to Prometheus

## Deployment Architecture

### Helm Chart Structure

```
helm/videocall-synthetic-clients/
├── Chart.yaml
├── values.yaml
├── values-production.yaml
├── values-development.yaml
└── templates/
    ├── deployment.yaml
    ├── configmap.yaml
    ├── service.yaml
    ├── servicemonitor.yaml
    ├── persistentvolume.yaml (for media files)
    └── _helpers.tpl
```

### Configuration Management

#### Helm Values (`values.yaml`)

```yaml
# Deployment Configuration
replicaCount: 5

image:
  repository: securityunion/videocall-synthetic-client
  pullPolicy: Always
  tag: "latest"

# Synthetic Client Configuration
clients:
  # Configuration per replica
  clientsPerPod: 10
  
  # Meeting Configuration
  meetingId: "test-meeting-001"
  serverUrl: "https://webtransport-us-east.webtransport.video"
  
  # Media Configuration
  audio:
    enabled: true
    type: "synthetic"  # synthetic | file
    config:
      frequency: 440  # Hz for synthetic tone
      filePath: "/assets/audio/test.wav"  # for file-based
  
  video:
    enabled: true
    type: "procedural"  # procedural | file | images
    config:
      pattern: "color_bars"  # color_bars | gradient | checkerboard
      resolution: "1280x720"
      framerate: 30
      bitrate: 500  # kbps
      filePath: "/assets/video/"  # for file-based

# Kubernetes Resources
resources:
  limits:
    cpu: "500m"
    memory: "512Mi"
  requests:
    cpu: "200m"
    memory: "256Mi"

# Monitoring
metrics:
  enabled: true
  port: 9090
  interval: "30s"

# Storage for media assets
persistence:
  enabled: true
  size: "5Gi"
  storageClass: "standard"

# Autoscaling
autoscaling:
  enabled: true
  minReplicas: 1
  maxReplicas: 20
  targetCPUUtilizationPercentage: 70
  targetMemoryUtilizationPercentage: 80

# Service Configuration
service:
  type: ClusterIP
  port: 8080
  metricsPort: 9090
```

#### Deployment Template (`templates/deployment.yaml`)

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "videocall-synthetic-clients.fullname" . }}
  labels:
    {{- include "videocall-synthetic-clients.labels" . | nindent 4 }}
spec:
  replicas: {{ .Values.replicaCount }}
  selector:
    matchLabels:
      {{- include "videocall-synthetic-clients.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "videocall-synthetic-clients.selectorLabels" . | nindent 8 }}
    spec:
      containers:
      - name: synthetic-client
        image: "{{ .Values.image.repository }}:{{ .Values.image.tag }}"
        imagePullPolicy: {{ .Values.image.pullPolicy }}
        env:
        - name: CLIENTS_PER_POD
          value: "{{ .Values.clients.clientsPerPod }}"
        - name: MEETING_ID
          value: "{{ .Values.clients.meetingId }}"
        - name: SERVER_URL
          value: "{{ .Values.clients.serverUrl }}"
        - name: AUDIO_ENABLED
          value: "{{ .Values.clients.audio.enabled }}"
        - name: VIDEO_ENABLED
          value: "{{ .Values.clients.video.enabled }}"
        - name: POD_NAME
          valueFrom:
            fieldRef:
              fieldPath: metadata.name
        - name: POD_NAMESPACE
          valueFrom:
            fieldRef:
              fieldPath: metadata.namespace
        ports:
        - containerPort: 8080
          name: http
        - containerPort: 9090
          name: metrics
        resources:
          {{- toYaml .Values.resources | nindent 12 }}
        volumeMounts:
        {{- if .Values.persistence.enabled }}
        - name: media-assets
          mountPath: /assets
        {{- end }}
        - name: config
          mountPath: /config
      volumes:
      {{- if .Values.persistence.enabled }}
      - name: media-assets
        persistentVolumeClaim:
          claimName: {{ include "videocall-synthetic-clients.fullname" . }}-media
      {{- end }}
      - name: config
        configMap:
          name: {{ include "videocall-synthetic-clients.fullname" . }}-config
```

## Implementation Plan

### Phase 1: Enhanced Bot Application (Week 1)

1. **Enhance Existing Bot Project**
   - Modify `bot/` to support WebTransport instead of WebSocket
   - Add `web-transport-quinn` dependency
   - Replace WebSocket connection with WebTransport session

2. **Add Media Streaming Capabilities**  
   - **Audio Producer**: Implement WAV file reading + Opus encoding (based on `neteq_player.rs`)
   - **Video Producer**: Copy `TestPatternSender` logic for JPEG sequence + VP9 encoding
   - **Timing**: 20ms audio packets (50fps), 33ms video packets (30fps)

3. **Per-Client Configuration System**
   - Parse JSON/YAML array for multi-client configurations
   - Support individual meeting IDs and media settings per client

### Phase 2: Containerization & Deployment (Week 2)

1. **Docker Container**
   - Create Dockerfile with pre-loaded media assets
   - Optimize for fast startup and low resource usage
   - Test multi-client scenarios

2. **Helm Chart**
   - Simple deployment template based on `rustlemania-websocket`
   - ConfigMap for client configuration
   - PersistentVolume for media assets
   - Horizontal scaling support

## Technical Specifications

### Dependencies

**Core Dependencies**:
```toml
[dependencies]
videocall-types = { path = "../videocall-types", version = "3.0.1" }
videocall-client = { path = "../videocall-client", version = "2.0.0" }  # For QUIC client
tokio = { version = "1.32.0", features = ["full"] }
quinn = "0.10.2"
anyhow = "1.0"
tracing = "0.1.37"
tracing-subscriber = { version = "0.3.17", features = ["env-filter"] }
clap = { version = "4.0.32", features = ["derive"] }
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"

# Media processing
opus = "0.3.0"
env-libvpx-sys = { version = "5.1.3", features = ["generate"] }
image = "0.25.5"
hound = "3.5"  # For WAV file handling

# Metrics
prometheus = "0.13"
axum = "0.7"  # For metrics HTTP server

# Async utilities
futures = "0.3"
rand = "0.8"
```

### Performance Targets

**Per Synthetic Client**:
- Memory usage: < 50MB
- CPU usage: < 100m (0.1 CPU core)
- Network bandwidth: Configurable (default 500kbps video + 64kbps audio)

**Scalability Targets**:
- 10 clients per pod (500MB memory limit)
- 100 pods per cluster (1000 clients total)
- Sub-100ms client startup time
- < 1% resource overhead for metrics collection

### File Formats and Assets

**Supported Audio Formats**:
- WAV (uncompressed)
- MP3 (via decoder)
- Raw PCM

**Supported Video Formats**:
- JPEG image sequences
- Raw RGB/YUV frames
- MP4 (via decoder) - future enhancement

**Asset Organization**:
```
/assets/
├── audio/
│   ├── conversation-sample.wav
│   ├── ambient-noise.wav
│   └── silence.wav
├── video/
│   ├── sequences/
│   │   ├── talking-head/
│   │   │   ├── frame_001.jpg
│   │   │   ├── frame_002.jpg
│   │   │   └── ...
│   │   └── presentation/
│   │       ├── slide_001.jpg
│   │       └── ...
│   └── patterns/
│       ├── test-pattern.jpg
│       └── color-bars.jpg
└── config/
    ├── profiles/
    │   ├── office-meeting.yaml
    │   ├── webinar.yaml
    │   └── social-call.yaml
    └── scenarios/
        ├── join-leave-pattern.yaml
        └── load-test-scenario.yaml
```

## Usage Examples

### Basic Deployment

```bash
# Deploy 50 synthetic clients to test-meeting-001
helm install synthetic-test ./helm/videocall-synthetic-clients \
  --set replicaCount=5 \
  --set clients.clientsPerPod=10 \
  --set clients.meetingId="test-meeting-001" \
  --set clients.serverUrl="https://webtransport-us-east.webtransport.video"
```

### Advanced Configuration

```bash
# High-scale load test with custom configuration
helm install load-test ./helm/videocall-synthetic-clients \
  --values ./configs/load-test-values.yaml \
  --set autoscaling.maxReplicas=50 \
  --set clients.video.config.bitrate=1000
```

### Monitoring

```bash
# View real-time metrics
kubectl port-forward svc/synthetic-test-metrics 9090:9090
# Access Prometheus metrics at http://localhost:9090/metrics
```

## Risk Analysis and Mitigation

### Technical Risks

1. **Resource Exhaustion**
   - *Risk*: High memory/CPU usage with many clients
   - *Mitigation*: Implement resource monitoring, horizontal pod autoscaling, configurable limits

2. **Network Saturation**
   - *Risk*: Too much synthetic traffic overwhelming network
   - *Mitigation*: Configurable bitrate limits, traffic shaping, gradual ramp-up

3. **Encoding Performance**
   - *Risk*: VP9/Opus encoding bottlenecks
   - *Mitigation*: Use fastest encoding settings, pre-encoded content caching, hardware acceleration

### Operational Risks

1. **Test Infrastructure Impact**
   - *Risk*: Synthetic clients affecting production systems
   - *Mitigation*: Dedicated test environments, traffic isolation, clear labeling

2. **Cost Overruns**
   - *Risk*: High cloud costs from large-scale testing
   - *Mitigation*: Resource quotas, automatic cleanup, cost monitoring alerts

## Success Criteria

### Functional Success Criteria

- [ ] Deploy 1000+ concurrent synthetic clients
- [ ] Generate realistic audio/video traffic patterns  
- [ ] Support multiple meeting rooms simultaneously
- [ ] Provide real-time performance metrics
- [ ] Successfully stress test videocall-rs infrastructure

### Performance Success Criteria

- [ ] < 50MB memory per synthetic client
- [ ] < 100ms client startup time
- [ ] 99.9% client connection success rate
- [ ] Accurate traffic generation (within 5% of target bitrate)
- [ ] Metrics collection with < 1% overhead

### Operational Success Criteria

- [ ] One-command deployment via Helm
- [ ] Comprehensive monitoring and alerting
- [ ] Graceful scaling and shutdown
- [ ] Clear documentation and usage examples
- [ ] Cost-effective cloud resource utilization

## Future Enhancements

### Phase 2 Features

1. **Advanced Media Simulation**
   - WebRTC simulcast support
   - Adaptive bitrate streaming
   - Network condition simulation (packet loss, jitter)

2. **Behavioral Simulation**
   - Realistic join/leave patterns
   - Speaking turn management
   - Screen sharing simulation

3. **Integration Testing**
   - End-to-end test automation
   - Performance regression testing
   - Chaos engineering integration

### Long-term Vision

- AI-powered realistic conversation simulation
- Cross-platform client behavior simulation
- Advanced network topology simulation
- Integration with continuous deployment pipelines

---

## Next Steps

1. **Design Review** - Review this document with the team
2. **Architecture Approval** - Confirm technical approach
3. **Resource Planning** - Allocate development time and infrastructure
4. **Implementation Start** - Begin Phase 1 development

This design provides a comprehensive foundation for building a production-ready synthetic client testing tool that will enable thorough validation of videocall-rs at scale.
