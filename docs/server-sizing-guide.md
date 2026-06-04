# Videocall-rs Server Sizing Guide

Single-node (non-HA) Kubernetes deployment sizing for videocall-rs.

Based on load testing with 20-participant bot meetings using realistic VP9 video
(costume recordings at 30fps/1000kbps) and Opus audio with DTX. Measurements
taken April 2026 on HCL K3s daily build cluster with 2-meeting and 3-meeting
simultaneous load tests.

## Sizing Formula

### CPU (cores)

```
Total cores = K8s base + Monitoring + App infra + Meeting load

K8s base:       0.5 cores   (kubelet, kube-proxy, containerd, node-exporter)
Monitoring:     0.2 cores   (Prometheus @ 15s scrape + Grafana)
App infra:      0.1 cores   (meeting-api + postgres + metrics-api + NATS idle + UI)
Meeting load:   0.2 cores × N   (WS relay + NATS fan-out per 20-person meeting)

Total = 0.8 + (0.2 × N)
```

### Memory (GiB)

```
Total RAM = K8s base + Monitoring + App infra + Meeting load

K8s base:       1.5 GiB     (kubelet, containerd, system)
Prometheus:     0.5 GiB     (grows ~50 MiB per active meeting from metric series)
Grafana:        0.1 GiB
App infra:      0.2 GiB     (postgres + meeting-api + metrics-api + UI)
NATS:           0.1 GiB
Relay per mtg:  0.025 GiB × N   (~25 MiB per 20-person meeting, both relays)

Total = 2.4 + (0.075 × N)
```

## Quick Reference Table

| Simultaneous meetings (20 users each) | CPU (cores) | RAM (GiB) | Recommended node |
|---|---|---|---|
| 1 | 1.0 | 2.5 | 2 core / 4 GiB |
| 5 | 1.8 | 2.8 | 2 core / 4 GiB |
| 10 | 2.8 | 3.2 | 4 core / 4 GiB |
| 15 | 3.8 | 3.5 | 4 core / 8 GiB |
| 20 | 4.8 | 3.9 | 6 core / 8 GiB |
| 50 | 10.8 | 6.2 | 12 core / 8 GiB |

## Measurement Basis

| Metric | Value | Source |
|---|---|---|
| Per-meeting relay CPU (WS) | ~170m steady state | 3-meeting tests (avg of 2 runs) |
| Per-meeting NATS CPU | ~30m steady state | 3-meeting tests |
| Per-meeting relay memory | ~25 MiB | cAdvisor during 20-bot meeting |
| Prometheus memory growth | ~50 MiB per meeting | 420 health metric series per 20-person meeting |
| Scaling behavior | Sub-linear | 3 meetings cost less per-meeting than 2 |

## Caveats

**CPU is the binding constraint.** Memory is almost irrelevant for the relay — it
forwards packets without buffering. Even at 50 meetings the relay memory is under
2 GiB.

**Conservative estimates.** Bot participants send at maximum bitrate (1000 kbps VP9)
and never adapt quality downward. Real users trigger adaptive quality within
minutes of a large meeting, reducing their bitrate to 150-600 kbps. Real meetings
will use less CPU than these estimates.

**WebSocket only.** These numbers are validated for the WS relay path. The WT relay
has similar per-meeting cost but bot-side WT inbound reception has a known bug
(accept_uni issue) that prevented full bidirectional WT load testing. Verify with
real WT clients before sizing WT-heavy deployments.

**Network bandwidth.** Not measured in these tests but can be estimated:
each 20-person meeting generates ~20 × 19 × 200 kbps ≈ 76 Mbps of relay egress
at typical AQ-adapted bitrates. At 50 meetings that's ~3.8 Gbps — factor NIC
capacity for large deployments.

**Client-side limit.** The server can handle many more meetings than any single
client can handle streams. Browser VP9 decode overload triggers AQ death spiral
at ~15 simultaneous video streams regardless of server capacity. Large meetings
(20+ participants) need selective forwarding or simulcast to work well.

**Single-node only.** For production HA deployments, plan for at least 2 nodes and
account for one node failing (N+1 capacity).

## How These Numbers Were Obtained

Load testing used the videocall-rs bot (`bot/` crate) with costume video
(pre-recorded Google Meet costume filter clips normalized to 1280x720 I420 frames).
Each bot sends VP9 at 30fps/1000kbps + Opus audio at 48kHz with DTX silence
suppression, and sends health packets every 1s with quality scores.

Tests were run on the HCL K3s daily build cluster (app.videocall.fnxlabs.com)
with Prometheus at 15s scrape intervals. CPU was measured from cAdvisor
`container_cpu_usage_seconds_total` counters using instant queries at known
timestamps with manual rate calculation.

See `bot/README.md` for bot usage and the bot improvement plan in project memory
for planned enhancements.

---

## Screen Share Bandwidth

Screen sharing has a fundamentally different bandwidth profile from camera video:

- **Higher steady-state bitrate.** Text and code are high-frequency detail that compresses
  poorly at camera-video bitrates. The high tier targets 2500 kbps vs. ~1000 kbps for camera.
- **Variable burst demand.** VBR mode lets the encoder burst **above** `max_bitrate_kbps`
  during scroll events and then drop back during static content. `max_bitrate_kbps` is the
  ceiling the AQ controller passes to `VideoEncoderConfig.bitrate` — it is not a hard cap on
  encoder output. See [VBR overshoot analysis](#vbr-overshoot-during-scroll-heavy-content)
  below.
- **Server-relay fan-out.** The relay forwards the presenter's stream to every other
  participant — relay egress scales as `(N-1) × bitrate`. This is the binding constraint
  at large N.

### Per-Tier Bitrate Cost Per Receiver

Derived from `SCREEN_QUALITY_TIERS` in
`videocall-client/src/adaptive_quality_constants.rs`. All values are relay egress
per viewer after the relay receives one inbound stream from the presenter.

| Tier | Resolution | FPS | Steady (ideal)¹ | AQ maximum¹ | VBR burst estimate² | Keyframe interval |
|------|-----------|-----|----------------|-------------|---------------------|-------------------|
| **high** | 1920×1080 | 10 | 2500 kbps | 4000 kbps | 3750–7500 kbps | ~3 s |
| **medium** | 1280×720 | 8 | 1200 kbps | 2000 kbps | 1800–3600 kbps | ~3 s |
| **low** | 1280×720 | 5 | 500 kbps | 1000 kbps | 750–1500 kbps | ~3 s |

¹ **Steady (ideal)** = `ideal_bitrate_kbps` in the tier definition. This is the maximum
  bitrate the AQ controller's PID will ever pass to `VideoEncoderConfig.set_bitrate()`.
  The PID is a reduction-only controller: it starts at `ideal_bitrate_kbps` and can only
  lower the target when network conditions degrade. It never drives the target above `ideal`.
  **AQ maximum** = `max_bitrate_kbps` in the tier definition. This is a defensive tier-clamp
  upper bound in the PID output path. Because `ideal ≤ max` for every tier, the upper
  clamp is mathematically unreachable in normal operation. It exists as a guard against
  edge-case overflow (e.g., NaN propagation) and as a transition-safety bound. It does
  **not** represent an achievable bitrate in practice.

² **VBR burst estimate** = expected 1s-window peak during heavy scrolling. Chrome's VP9
  VBR encoder (libvpx) can produce 1.5–3× overshoot above the configured `set_bitrate()`
  target in any 1-second window. The configured target maxes out at `ideal_bitrate_kbps`,
  so burst estimates are 1.5–3× `ideal_bitrate_kbps`. See
  [VBR overshoot analysis](#vbr-overshoot-during-scroll-heavy-content) for derivation.

> **Capacity planning rule of thumb:** Use the **VBR burst estimate** column for NIC
> sizing. Use the **Steady (ideal)** column for normal-operation egress budgeting.

The relay is a pure forwarder — CPU cost for screen share is the same per-packet as
camera video. The additional load comes entirely from the higher sustained bitrate and
the increased NATS message rate.

### Meeting-Size Multiplier Table

1 presenter screen-sharing, N−1 viewers. Relay egress = (N−1) × tier_bitrate.
Steady = `ideal_bitrate_kbps` (the actual AQ controller ceiling). For NIC sizing,
use the VBR burst estimate column (1.5–3× steady). CPU overhead is dominated
by network I/O at these bitrates; memory overhead is negligible (relay does not buffer).

#### High tier (1920×1080, 10fps, ideal/AQ-max 2500 kbps / VBR burst est. 3750–7500 kbps)

| Meeting size N | Viewers (N−1) | Relay egress steady | Relay egress peak |
|---------------|--------------|--------------------|--------------------|
| 5 | 4 | 10 Mbps | 16 Mbps |
| 10 | 9 | 22.5 Mbps | 36 Mbps |
| 20 | 19 | 47.5 Mbps | 76 Mbps |
| 50 | 49 | 122.5 Mbps | 196 Mbps |
| 100 | 99 | 247.5 Mbps | 396 Mbps |

#### Medium tier (1280×720, 8fps, ideal/AQ-max 1200 kbps / VBR burst est. 1800–3600 kbps)

| Meeting size N | Viewers (N−1) | Relay egress steady | Relay egress peak |
|---------------|--------------|--------------------|--------------------|
| 5 | 4 | 4.8 Mbps | 8 Mbps |
| 10 | 9 | 10.8 Mbps | 18 Mbps |
| 20 | 19 | 22.8 Mbps | 38 Mbps |
| 50 | 49 | 58.8 Mbps | 98 Mbps |
| 100 | 99 | 118.8 Mbps | 198 Mbps |

#### Low tier (1280×720, 5fps, ideal/AQ-max 500 kbps / VBR burst est. 750–1500 kbps)

| Meeting size N | Viewers (N−1) | Relay egress steady | Relay egress peak |
|---------------|--------------|--------------------|--------------------|
| 5 | 4 | 2 Mbps | 4 Mbps |
| 10 | 9 | 4.5 Mbps | 9 Mbps |
| 20 | 19 | 9.5 Mbps | 19 Mbps |
| 50 | 49 | 24.5 Mbps | 49 Mbps |
| 100 | 99 | 49.5 Mbps | 99 Mbps |

### Mixed-Mode Scenarios

Real meetings combine screen share with camera and audio. Use these additive
estimates per presenter slot:

```
Relay egress per participant slot (steady state):
  Camera video (AQ-adapted, typical):   ~600 kbps
  Audio (Opus 50kbps, DTX active ~60%): ~20 kbps
  Screen share — high tier:            2500 kbps
  Screen share — medium tier:          1200 kbps

Full presenter (camera + screen + audio), high tier:
  3120 kbps per viewer — or ~3.1 Mbps per viewer

Full presenter (camera + screen + audio), medium tier:
  1820 kbps per viewer — or ~1.8 Mbps per viewer
```

**Example: 20-person webinar, 1 presenter with camera + screen (high tier)**

```
Camera + screen + audio = 3120 kbps
19 viewers × 3120 kbps = 59.3 Mbps relay egress (steady)
19 viewers × (600+3750+50) kbps = 80.75 Mbps relay egress (VBR burst est. 1.5× ideal)
19 viewers × (600+7500+50) kbps = 155.7 Mbps relay egress (VBR burst est. 3× ideal)
```

**Example: 2 simultaneous presenters (both screen-sharing, high tier), 18 viewers**

The relay receives 2 independent screen-share streams and fans each out to all other
participants (N−1 = 19 each, including each presenter receiving the other's share):

```
Each screen share stream → forwarded to 19 peers
2 streams × 19 peers × 2500 kbps =  95 Mbps relay egress (steady)
2 streams × 19 peers × 5000 kbps = 190 Mbps relay egress (VBR burst est. 2× ideal)
```

### Increase vs. Camera-Only Baseline

At high tier, adding one screen-share presenter to a camera-only meeting increases
relay egress by approximately:

```
Camera-only 20-person meeting:  19 streams × 19 viewers × 600 kbps ≈ 216 Mbps
Add 1 screen-share presenter:   + 19 viewers × 2500 kbps ≈ +47.5 Mbps
Increase:                       +22% steady
                                 up to +35% at VBR burst est. (3750 kbps, 1.5× ideal)
```

At medium tier the increase is ~11% steady. At high tier with VBR burst the
egress model predicts a **~55% fan-out increase per active screen-share slot**
relative to a single camera-only participant slot during heavy scroll events
(down from the ~100%+ figure in the previous version of this doc, which incorrectly
used `max_bitrate_kbps` as the AQ ceiling).

> **Operator callout (see also runbook):** Monitor `relay_outbound_bytes_total`
> rate when screen sharing is in use. A single presenter at high tier accounts
> for the same egress as ~4 camera-only participants at steady state, and up to
> ~6–12 during burst scroll events (1.5–3× overshoot over the 2500 kbps target).
> Alert threshold for relay NIC saturation should be revised to account for this.
> See VBR overshoot analysis below.

### VBR Overshoot During Scroll-Heavy Content

**This is the most important section for capacity planning.** The screen encoder uses
`bitrateMode = "variable"`, meaning the VP9 encoder can burst well above the configured
`set_bitrate()` target during scroll events. Understanding the actual `set_bitrate()` range
is essential to sizing correctly.

#### What value is actually passed to `VideoEncoderConfig.set_bitrate()`

The AQ controller (`EncoderBitrateController`) maintains an `ideal_bitrate_kbps` that
starts at the current tier's `ideal_bitrate_kbps` and is updated on each tier transition.
The PID is a **reduction-only** controller:

```
after_pid = ideal_bitrate_kbps × (1 − reduction_pct)
  where  reduction_pct ∈ [0, 0.9]
```

So `after_pid ≤ ideal_bitrate_kbps` always. The result is further clamped to
`[min_bitrate_kbps, max_bitrate_kbps]`, but since `ideal ≤ max` for every tier, the
upper clamp **never fires** in normal operation.

Consequence: the maximum value ever passed to `set_bitrate()` is `ideal_bitrate_kbps`,
not `max_bitrate_kbps`. The `max_bitrate_kbps` tier field exists as a defensive guard
against edge-case NaN/overflow in the PID output path only.

| Tier | Tier `ideal` | Tier `max` | Actual `set_bitrate()` max | `max` reachable? |
|------|-------------|-----------|--------------------------|------------------|
| high | 2500 kbps | 4000 kbps | **2500 kbps** | No |
| medium | 1200 kbps | 2000 kbps | **1200 kbps** | No |
| low | 500 kbps | 1000 kbps | **500 kbps** | No |

#### Why `VideoEncoderConfig.bitrate` is not a hard cap on encoder output

The WebCodecs `VideoEncoderConfig.set_bitrate(target_bps)` field is a **soft average
target**, not a per-frame hard ceiling. Chrome's VP9 implementation (libvpx in VBR mode)
enforces rate control by averaging over a sliding window, not per-frame. Individual frames
— particularly the first frame after a fast scroll — can require encoding a large fraction
of the screen's changed macroblocks, producing a frame 10–40× larger than the steady-state
per-frame budget. libvpx compensates over subsequent frames, so the **long-run average**
stays near `set_bitrate()` target, but **1-second window measurements** can significantly
exceed it.

#### Analytical overshoot estimate for scroll-heavy content

The `set_bitrate()` target maxes out at `ideal_bitrate_kbps`. Chrome's VP9 VBR allows
approximately 1.5–3× overshoot above the configured target over a 1s window during
high-motion events (source: libvpx VBR rate control documentation and published
measurements from WebRTC implementations using the same VP9 codec stack).

| Tier | `set_bitrate()` max (`ideal`) | Expected 1s-window peak | Exceeds 1.5× threshold? |
|------|------------------------------|------------------------|------------------------|
| **high** | 2500 kbps | 3750–7500 kbps | **Yes** (threshold: 3750 kbps) |
| **medium** | 1200 kbps | 1800–3600 kbps | **Yes** (threshold: 1800 kbps) |
| **low** | 500 kbps | 750–1500 kbps | **Likely** (threshold: 750 kbps) |

All three tiers are analytically expected to exceed the 1.5× threshold during intense
scroll events. Empirical confirmation is pending (see measurement protocol below).

#### Proposed mitigations

**Option A — Switch low tier to CBR (recommended for constrained networks)**

The low tier (`max_bitrate_kbps = 1000`) is used precisely when the network is
constrained, making the bandwidth ceiling load-bearing. Switching to `bitrateMode =
"constant"` eliminates overshoot at the cost of reduced quality during scroll frames
(the encoder must distribute the budget evenly rather than bursting).

```rust
// In screen_encoder.rs — change set_vbr_mode to set_cbr_mode for low tier
fn set_cbr_mode(config: &VideoEncoderConfig) {
    let _ = Reflect::set(
        config,
        &JsValue::from_str("bitrateMode"),
        &JsValue::from_str("constant"),
    );
}
// When configuring: if tier_index == SCREEN_QUALITY_TIERS.len() - 1 { set_cbr_mode }
// else { set_vbr_mode }
```

**Option B — Reduce `ideal_bitrate_kbps` to absorb overshoot budget**

If empirical measurements confirm a 2× overshoot factor, halve the `ideal_bitrate_kbps`
values so the 1s-window burst lands near the current expected ceiling. Note that
`max_bitrate_kbps` should be reduced proportionally (it must stay ≥ `ideal`) but has no
functional effect on the AQ controller output:

```rust
// Adjusted tier table if 2× VBR overshoot is confirmed:
// high:   ideal_bitrate_kbps: 1250, max_bitrate_kbps: 2000   (burst ~2500 kbps)
// medium: ideal_bitrate_kbps:  600, max_bitrate_kbps: 1000   (burst ~1200 kbps)
// low:    ideal_bitrate_kbps:  250, max_bitrate_kbps:  500   (burst ~500 kbps)
```

Note: this halves steady-state quality for dynamic content, which is the primary
use-case for screen share. Not recommended without measurement data.

**Option C — Use updated burst-column figures for relay NIC sizing (no code change)**

If the VBR quality benefit outweighs the capacity planning inaccuracy, keep the current
encoder configuration and update only the capacity planning tables to use the burst
column (above) for NIC sizing. This is the lowest-risk option if relay NICs have
headroom; it is not safe if relay egress is already near saturation.

**Current recommendation:** Implement **Option A** for low tier only, measure all three
tiers per the protocol below, and re-evaluate high and medium once empirical data is
available. Do not implement Option B without measurement confirmation.

### Measurement Status

**These numbers are model-only.** The bot crate does not yet implement a
screen-share producer (it sends camera-style VP9 only). Empirical validation
against a live deployment has not been performed.

#### First-principles static-content bitrate analysis

Running a live screen-share session in a browser to measure via
chrome://webrtc-internals requires direct browser interaction and cannot be
automated here. Instead, this section derives expected steady-state bitrates
analytically from the encoder configuration and VP9 behavior, identifies where
the `ideal_bitrate_kbps` figures in the tier table may be misleading, and
provides a precise measurement protocol.

**VP9 VBR + static content: the keyframe-dominance regime**

VP9 in VBR mode (`bitrateMode = "variable"`) produces near-zero-byte delta
frames for completely static screen content — a perfect skip costs ~1-5 bytes
per frame. But every scheduled keyframe must encode the full screen. For a
code editor or terminal window at 1080p, a VP9 keyframe encoded at quality
settings appropriate for 2500 kbps typically costs **100–300 KB** depending on
syntactic complexity (syntax highlighting = more edges = larger keyframe).

With a 3-second keyframe interval:

| Tier | Resolution | FPS | Keyframe interval | Est. keyframe cost | Est. avg bitrate (static) |
|------|-----------|-----|------------------|--------------------|--------------------------|
| **high** | 1920×1080 | 10 | 3 s (30 frames) | 150–300 KB | **400–800 kbps** |
| **medium** | 1280×720 | 8 | 3 s (24 frames) | 80–160 KB | **213–427 kbps** |
| **low** | 1280×720 | 5 | 3 s (15 frames) | 80–160 KB | **213–427 kbps** |

**How this compares to `ideal_bitrate_kbps`:**

- High tier ideal = 2500 kbps. Static-content average ≈ **400–800 kbps — 68–84%
  below ideal.** The `ideal_bitrate_kbps` reflects dynamic content (scrolling,
  typing bursts). For capacity planning, static-content presenters will use
  far less bandwidth than the ideal figure suggests.
- Medium tier ideal = 1200 kbps. Static average ≈ **213–427 kbps — 64–82% below.**
- Low tier ideal = 500 kbps. Static average ≈ **213–427 kbps — 0–15% below.**
  Low tier is the odd case: the keyframe cost alone may approach the ideal bitrate,
  meaning the low tier's bitrate budget is keyframe-dominated at all times.

**The `min_bitrate_kbps` floor prevents under-spending**

The encoder config sets `min_bitrate_kbps` (high: 1500, medium: 700, low: 250).
WebCodecs VBR mode treats this as a soft floor. In practice Chrome's VP9 encoder
honours `min_bitrate_kbps` primarily by increasing quantiser on keyframes rather
than inserting padding, so actual wire bitrate may dip below the minimum during
long static periods between keyframes. This makes the static-content average
**unpredictable** without measurement — it lies somewhere between the keyframe-only
estimate above and the `min_bitrate_kbps` floor.

**Implication for capacity planning**

The sizing guide's tables (above) use `ideal_bitrate_kbps` as the steady figure.
For deployments where presenters share mostly-static content (code reviews, slides,
dashboards), the real steady-state may be 3–6× lower than the ideal column.
**Use the ideal column for worst-case (active-scrolling) sizing; use 30–40% of ideal
for static-content planning.** The VBR burst estimate column (1.5–3× ideal) is the right
value for NIC sizing — see VBR overshoot analysis above.

#### Measurement protocol (to be run manually)

To replace the estimates above with measured values:

**Part 1 — Static content (average bitrate)**

1. Join a meeting in the local dev stack (`make up`, then open two browser tabs).
2. In one tab, start screen sharing a code editor or terminal (static content).
   Keep the content motionless for the measurement window.
3. Open `chrome://webrtc-internals` in a second Chrome window.
4. Navigate to the sending peer's entry → `RTCOutboundRtpVideoStream` stats.
5. Record `bytesSent` at T=30s and T=210s after screen share starts
   (avoids the 8-second `SCREEN_QUALITY_WARMUP_MS` warmup and first keyframes).
6. Average bitrate = `(bytesSent_210 - bytesSent_30) / 180 * 8 / 1000` kbps.

**Part 2 — Scroll-heavy content (peak instantaneous bitrate)**

1. Same setup as Part 1. Use the same `chrome://webrtc-internals` stats view.
2. After T=30s (past warmup), perform intensive scrolling through a long document:
   rapid mouse-wheel scrolling through a 1000+ line file, or rapid window switching
   between applications, for a continuous 60-second burst.
3. While scrolling, in `chrome://webrtc-internals`, watch the `bytesSent` counter.
   For instantaneous bitrate: record `bytesSent` at two consecutive 1-second
   snapshots (the stats panel updates ~1/s): `(S2 - S1) * 8 / 1000` kbps.
4. Note the highest observed 1s-window value during the scroll burst.
5. Also record `qualityLimitationReason` — if the encoder reports
   `"bandwidth"` limitation during the burst, that confirms VBR is saturating.
6. Acceptance threshold: if peak > `1.5 × ideal_bitrate_kbps` (3750/1800/750 kbps for
   high/medium/low), implement Option A (CBR for low tier) and/or update the
   burst-column figures in the tier table.

**Part 3 — Force-specific tier**

Repeat Parts 1 and 2 for each tier by temporarily patching
`DEFAULT_SCREEN_TIER_INDEX` in `adaptive_quality_constants.rs`:
- `0` = high, `1` = medium, `2` = low.
Rebuild with `trunk build --features ...` and repeat the steps.

Update the table below with results.

#### Pending measurements

| Tier | Avg bitrate (static) | Avg bitrate (active scroll) | Peak 1s bitrate (scroll) | Exceeds 1.5× threshold? | Date | Notes |
|------|---------------------|-----------------------------|--------------------------|------------------------|------|-------|
| high | — | — | — | — | — | Pending browser session |
| medium | — | — | — | — | — | Pending browser session |
| low | — | — | — | — | — | Pending browser session |

The `relay_outbound_bytes_total` Prometheus counter is the correct server-side
metric to validate this model. The procedure to validate is:

1. Deploy a bot meeting with 1 bot sending screen-share-tier bitrates and N−1
   bots receiving (observer mode).
2. Record `rate(relay_outbound_bytes_total[1m]) * 8` at steady state.
3. Compare against the table above for the appropriate N and tier.
4. Update this section with the `(N, tier, measured_Mbps, timestamp)` measurement tuple.

**Pending measurement:** N ≥ 10 run from the bot fleet using a screen-share-capable
bot producer. Tracked in the bot improvement plan. This section will be updated with
real data when that run is completed.

