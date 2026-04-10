# Meeting Analysis: Room `cc7tp` — 2026-04-08 15:26–15:34 UTC

**Duration:** ~8.5 minutes | **Peak participants:** 15 | **Avg participants:** 11.1 | **Total peer connections:** 301

**Cluster:** HCL K3s (grafana.videocall.fnxlabs.com)

## Overall Quality

90% of quality scores were perfect (100), with 4% good and only 4% degraded or poor. The meeting worked well for the vast majority of participants. However, a handful of peers had seriously degraded experiences — the worst 5 averaged scores of 2.1, 5.5, 6.5, 7.6, and 14.9 (out of 100).

## Key Findings

### 1. Audio packet loss metric is broken (Bug)

All 208 audio peers reported non-zero packet loss, with peaks showing impossible values like 10,038%. This is almost certainly a metrics calculation bug — likely a rate/counter reset issue rather than actual packet loss. If loss were genuinely that high, audio would have been completely unintelligible.

**Action:** Investigate the packet loss formula in the client health reporting code.

### 2. High jitter buffers for ~half of peers (Monitor)

Average NetEQ audio buffer was 357ms across 253 peers, with 123 peers (49%) exceeding 200ms. The worst peer hit 3,560ms. High jitter buffers indicate the network path had significant packet timing variation. This is consistent with a geographically distributed group — buffers grow to absorb jitter, trading latency for continuity.

### 3. Most peers had low video bitrate (Expected)

Average video bitrate was 292 Kbps, but 167 of 261 video peers (64%) averaged below 200 Kbps. With 15 participants, each client is receiving multiple video streams, so bandwidth gets divided. This is expected behavior for a large meeting, but it means video quality was likely noticeably compressed for most viewers.

### 4. Four high-latency participants (Info)

4 peers averaged RTT >200ms (max spike 354ms). These are likely on distant or congested networks. Median RTT was a reasonable 78ms.

### 5. Video FPS mostly solid

Average 26.6 FPS across peers. 24 peers dropped below 15 FPS at some point — likely correlated with the low-bitrate peers, as encoders reduce framerate when bandwidth-constrained.

### 6. User IDs all showing "?" (Gap)

Every `user_id` label in the metrics is `"?"`. User identification isn't being populated in the client health metrics, which makes it impossible to correlate quality issues to specific people. This is a known gap — the vcprobe branch has the instrumentation, but it hasn't merged to the branch running on this cluster yet.

## Other Rooms Active During This Period

| Room | Peak Participants |
|------|-------------------|
| cc7tp | 15 |
| meeting_sync | 5 |
| infra | 3 |
| 19d6db7f432 | 2 |

## Summary Table

| Issue | Severity | Action |
|-------|----------|--------|
| Packet loss metric showing impossible values | **Bug** | Investigate rate calculation in client health reporting |
| User IDs all "?" in metrics | **Gap** | Merge vcprobe branch to get proper user identification |
| 49% of peers with >200ms jitter buffer | **Monitor** | Expected for distributed participants; no fix needed |
| 64% of peers with <200 Kbps video | **Expected** | Bandwidth sharing across 15 streams; consider SFU optimizations for large meetings |
| 4 high-RTT peers | **Info** | Network-dependent; no server-side fix |

## Priority Actions

1. **Packet loss metric bug** — most actionable; polluting dashboard data and making it hard to assess real audio quality
2. **Missing user IDs** — merge vcprobe branch to enable per-participant troubleshooting
