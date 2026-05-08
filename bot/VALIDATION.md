# Bot Adaptive Quality + Network Impairment — Validation Log

Tracks what we've proven about the bot's ability to simulate real users with real network conditions. Goal: high confidence that a bot-driven load test accurately reflects what real browser users experience.

Update this file after every validation session. Add entries, don't overwrite.

## Feature scope under validation

Introduced in PR #564 on branch `feat/bot-adaptive-quality-and-netsim`:
1. Bots run the real `videocall-aq` adaptive-quality state machine (extracted from `videocall-client`).
2. Bots consume peer DiagnosticsPackets and adapt encoder tier/bitrate/FPS accordingly.
3. Bots populate HealthPacket tier fields truthfully (replacing a hard-coded `Some(0)` that lied to peers).
4. Transport-layer impairment shim applies per-participant latency/jitter/loss/bandwidth to WT and WS byte streams.
5. Presets: `good_wifi`, `good_4g`, `congested_wifi`, `lossy_mobile`, `satellite`, `dialup`, `none`.
6. CLI: `--impair-all <profile>`, `--impair-name <name>=<profile>`, `--no-impair`.

## Validation matrix

| # | What we need to prove | Status | Evidence |
|---|---|---|---|
| V1 | Impairment config reaches the shim with correct values | ✅ PROVEN | 2026-05-06 |
| V2 | Impaired bot is visibly lossier than healthy bot (bot-side RX stats) | ✅ PROVEN | 2026-05-06 |
| V3 | Impaired bot's own AQ steps down in response | ✅ PROVEN | 2026-05-06 |
| V4 | HealthPackets flow bot → relay → NATS → metrics-api → Prometheus | ✅ PROVEN | 2026-05-06 |
| V5 | Peers report LOWER call_quality_score for impaired bot than healthy bots | ✅ PROVEN | 2026-05-06 |
| V6 | Every preset (`good_wifi`, `good_4g`, ..., `dialup`) produces distinguishable signatures | ⏳ PARTIAL | only `lossy_mobile` tested |
| V7 | Impairment works on WebTransport (not just WebSocket) | ⏳ PENDING | run tested `wt_ratio=0.0` (WS only) |
| V8 | Impairment applies to BOTH directions (up + down) correctly | ⏳ PARTIAL | inbound shown to drop; outbound inferred but not directly measured |
| V9 | Real browser user joining a meeting with impaired bots sees the impaired bot as degraded | ⏳ PENDING | 2026-05-06 run had browser in room but no direct comparison capture |
| V10 | Multiple impaired bots with different profiles don't cross-pollute | ⏳ PENDING | only 1 impaired bot per run so far |
| V11 | Impairment is deterministic with `seed:` for reproducible tests | ⏳ PENDING | seed plumbing exists but unverified end-to-end |
| V12 | Passthrough mode (all-healthy fleet) has zero measurable perf overhead vs pre-PR | ⏳ PENDING | claim made in code review; not benchmarked |
| V13 | Bot populates `tier_transitions` list (counter events), not just gauge | ❌ GAP IDENTIFIED | 2026-05-06 — only browser populates it; bot gap confirmed in code |
| V14 | `audio_concealment_pct` populates meaningfully for lossy bot | ❓ UNCLEAR | 2026-05-06 — showed 0% even for alice; root cause not investigated |
| V15 | Simulated loss rate measured end-to-end matches configured loss_pct | ⏳ PENDING | netsim unit tests pass but not E2E validated |
| V16 | Costume video resolution limitation (720p fixed) doesn't break AQ decisions | ⚠️ KNOWN LIMITATION | documented; AQ logs warning when tier requests <720p |

## Test runs

### 2026-05-06 — first end-to-end on HCL daily

**Setup:**
- Binary: built from `feat/bot-adaptive-quality-and-netsim` @ `7d282fd3`, static libvpx.
- Host: `jenkins-volt-mx-go-3` (10.190.252.90).
- Cluster: HCL daily (`app.videocall.fnxlabs.com`, meeting `1`).
- Command: `./run.sh config-room1.yaml --users 10 --impair-name alice=lossy_mobile`
- Duration: ~3m14s (18:57:01 → 19:00:15 UTC).
- Participants: 10 bots (alice, bob, carol, dave, eve, frank, grace, henry, iris, jack) + 1 real browser (jay.boyd via WebTransport).
- Transport: WebSocket (wt_ratio=0.0 per config).

**Results:**

V1 — config reached shim correctly. Log line 742:
```
[alice] network impairment: latency=150ms jitter=50ms loss=5% up=Some(800)kbps down=Some(2000)kbps
```

V2 — alice observably lossier per bot `inbound_stats`:
- alice audio sequence gaps: median 452/10s interval, total 57,007
- alice video sequence gaps: median 717/10s, total 12,605
- other 9 bots (combined): audio median ~145/peer/interval, video median ~4/peer/interval
- ~170× more video gaps per peer for alice vs healthy bots

V3 — alice's AQ stepped down:
- First `AQ_STATUS` for alice: `corrected_bitrate` dropped 600 → 493 → 467 → 390 kbps within 1s of media start
- Over the run: `encoder_target_bitrate_kbps` per bot (from Prometheus):
  - alice: `410, 2388, 1907, 1500, 1597, 1200, 1402` kbps (PID fighting impairment)
  - bob:   `430, 2401, 1937, 1500, 1628, 1200, 800`
  - (alice's trajectory shows earlier/sharper PID reactions; mean bitrate much closer to 1200 than healthy ~1600+)
- Tier movement: every bot went `4 → 0 → 0 → 0 → 1 → 1 → 2`; alice followed the same pattern because the AQ is driven by peer feedback and her *peers* were fine receiving from HER (she was the one losing inbound)
- Costume video resolution downshift was blocked (logged warning): `tier requested 960x540 but costume resolution is fixed at 1280x720 in v1`

V4 — Prometheus pipeline confirmed working:
- `videocall_health_reports_total` incremented per bot
- `videocall_meeting_participants{meeting_id="1"} = 9` during the run
- `videocall_adaptive_video_tier{meeting_id="1"}` had 7-8 samples per bot
- `videocall_encoder_target_bitrate_kbps{meeting_id="1"}` populated per bot
- `videocall_call_quality_score{meeting_id="1"}` populated per (from_peer, to_peer) pair

V5 — **peer-perceived quality of alice vs others** (call_quality_score, avg over last 3m of run, averaged across all observers):

| Bot | Quality | Notes |
|---|---|---|
| jack | 97.6 | healthy baseline |
| henry | 96.8 | healthy baseline |
| iris | 90.2 | |
| bob | 89.8 | |
| eve | 86.8 | |
| dave | 84.7 | |
| grace | 84.5 | |
| carol | 74.4 | |
| frank | 68.7 | |
| **alice** | **64.7** | 🔻 impaired (`lossy_mobile`) |

Every peer scored alice **20–30 points lower** than bob. Unanimous signal. Network impairment is correctly perceived by the cluster's quality machinery.

**What went wrong during analysis (for future-me):**
- Wasted ~1 hour investigating "no Prometheus data for meeting 1" when in fact data was there all along; wrong time window.
- See `~/.claude/projects/-home-jboyd-work-videocall-rs/memory/prometheus-query-gotchas.md` for the fix: always `max_over_time[7d]` before concluding pipeline broken.
- UTC/EDT confusion: bot log timestamps are UTC, Chrome-saved browser logs are local EDT. Add 4h when comparing.

**Gaps discovered:**
- V13: bot doesn't populate `HealthPacket.tier_transitions` list (only the current-tier gauge). Browser does. Means `videocall_tier_transition_total` counter never increments for bot-originated meetings. Follow-up commit needed on feat branch.
- V14: `audio_concealment_pct` was 0 for alice despite her 5% inbound loss. Two plausible reasons: (a) it tracks decoder-side concealment (packets alice's peers missed from her upstream), and alice's uplink wasn't stressed enough to drop outbound packets; (b) bot software DTX (RMS<0.005 silence suppression) mutes enough that peers don't get audio to conceal. Not investigated further.

## Open validation work

### Near-term (next session)

- [ ] **V7 — WebTransport path.** Rerun with `wt_ratio=1.0` and alice on `lossy_mobile`. Confirm netsim shim is inserted in WT transport loop, not bypassed by the uni-stream / datagram split.
- [ ] **V6 — each preset.** Run 7 short (2-3 min) tests, one preset each, all 10 bots impaired identically. Record tier distribution + encoder bitrate + peer-perceived quality per preset. Build a reference table: "what does `dialup` look like?"
- [ ] **V9 — browser-as-ground-truth.** Have a real browser in the meeting with 3 bots: 1 passthrough, 1 `congested_wifi`, 1 `lossy_mobile`. Watch their tiles. Record subjective impressions alongside Prometheus metrics. Goal: confirm the numbers match human-observable video/audio quality differences.
- [ ] **V13 — fix bot tier_transitions reporting.** Wire `AdaptiveQualityManager::drain_transitions()` (or equivalent) into `bot/src/health_reporter.rs::build_health_wrapper`. Small commit on feat branch. Then re-run and confirm `videocall_tier_transition_total` counter increments for bots.
- [ ] **V14 — audio concealment investigation.** Increase alice's outbound impairment (lower uplink_kbps, higher loss_pct) and see if concealment registers. Also check whether the bot's software DTX is suppressing meaningful audio entirely.

### Medium-term

- [ ] **V10 — mixed impairment.** Alice on `satellite`, bob on `dialup`, carol on `lossy_mobile`, rest passthrough. Verify independent behavior; no cross-contamination of tier decisions.
- [ ] **V15 — loss rate E2E.** Set `loss_pct=10.0, seed=42` on alice. Count her inbound sequence-gap deltas across a 5-min run, divide by expected pkt count. Should be within ~1% of 10%.
- [ ] **V11 — determinism.** Run the same scenario twice with `seed:42`. Diff the bot logs (modulo timestamps). Drop-decisions should be identical.
- [ ] **V12 — passthrough overhead.** Benchmark: 50 passthrough bots on PR-staging binary vs PR #564 binary. Measure CPU% of the `bot` process. Target: within 3% of each other.

### Long-term / nice-to-have

- [ ] Add a `bot stress-test` make target that runs a curated suite of impairment profiles + records a Grafana snapshot for regression comparison.
- [ ] Add a `--impair-probability <fraction>` CLI flag so a random subset of bots gets impaired (currently need per-name declaration).
- [ ] Document what "realistic" looks like: reference link to a known-good Grafana dashboard showing real-browser users in a healthy vs degraded meeting for comparison.

## Reference commands

```bash
# Static build
VPX_LIB_DIR=/usr/lib/x86_64-linux-gnu VPX_INCLUDE_DIR=/usr/include \
  VPX_VERSION=1.11.0 VPX_STATIC=1 \
  cargo build --release -p bot

# Run with impairment
RUST_LOG=info ./run.sh config-room1.yaml --users 10 --impair-name alice=lossy_mobile

# Prometheus: sanity-check a metric exists at all
curl -sk "$PROM/api/v1/query" --data-urlencode 'query=max_over_time(videocall_adaptive_video_tier[7d])'

# Prometheus: peer-perceived quality of target bot
curl -sk "$PROM/api/v1/query" \
  --data-urlencode 'query=avg_over_time(videocall_call_quality_score{meeting_id="1",to_peer="alice"}[180s] @ <UTC_EPOCH>)'

# Convert local time to UTC epoch for Prometheus
date -u -d '2026-05-06T14:57:00 EDT' +%s
```

## Related files

- `bot/src/netsim.rs` — shim core (token bucket, drop, jitter, delay)
- `bot/src/netsim_profiles.rs` — preset definitions
- `bot/src/aq_controller.rs` — BotAq wrapper around videocall-aq
- `bot/src/health_reporter.rs` — tier fields populated here (see V13)
- `bot/tests/aq_degradation.rs` — deterministic in-process integration test
- `videocall-aq/` — shared AQ state machine (also used by browser)
