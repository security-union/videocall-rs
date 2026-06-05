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

Extended (#988, viewport fidelity): bots emit a `VIEWPORT` control packet so the
relay can filter VIDEO from off-screen sources (V17–V19).

Extended (#1083-A2, per-receiver simulcast): the bot validates the PUBLISH side +
the RELAY FILTER of per-receiver simulcast (#989 / #1079 / #1082). The bot is a
publisher with no receiver chooser, so it (a) publishes a multi-layer ladder when
`experimentalSimulcastMaxLayers` is raised, (b) runs its own
`uplink_budget_kbps` / AQ layer-shed under congestion, and (c) emits a synthetic
`LAYER_PREFERENCE` (`--pin-layer N` / `BOT_PIN_LAYER`) to validate the relay's
per-receiver layer filter — see V20–V22. Default OFF (no `--pin-layer` →
fail-open, existing behaviour unchanged).

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
| V17 | Bot `media_kind` lets the relay viewport-filter bot VIDEO (no longer optimistic) | 🟦 DESIGN COMPLETE — PENDING REAL RUN (cluster) | code done on `feat/988-bot-and-relay-observability`; unit tests + clippy green; STILL not exercisable without a live #988 relay. NOT cleared — same relay-filter path as V22, deploy-gated. |
| V18 | Bot `VIEWPORT` reduces measured inbound `video_bytes` end-to-end | 🟦 DESIGN COMPLETE — PENDING REAL RUN (cluster) | code + 5 `viewport_sender` unit tests green; E2E `video_bytes` drop STILL not measurable in-process — needs a #988-enabled relay. NOT cleared. |
| V19 | Bot re-asserts its `VIEWPORT` after a viewport-subscription loss (reconnect / re-election / relay idle) so filtering does not silently lapse | 🟦 DESIGN COMPLETE — PENDING REAL RUN (cluster) | `resend_on_reconnect()` + 4 unit tests green (re-send current set, no-send-when-never-sent, no-send-in-legacy, rate-limited); E2E recovery still NOT verifiable in-process — needs a #988-enabled relay on a cluster. NOT cleared. |
| V20 | Bot-as-publisher emits a multi-layer simulcast ladder when `experimentalSimulcastMaxLayers` is raised (publish side of per-receiver simulcast #989/#1082) | 🟦 DESIGN COMPLETE — DEPLOY-GATED | `videocall-aq` builds an `n`-layer ladder via `set_simulcast_layers`; bot stamps `PacketWrapper.simulcast_layer_id` per layer. Cluster capture deploy-gated on #1079+#1082. Run **twice: once WT, once WS.** |
| V21 | `uplink_budget_kbps` caps the SUM of active-layer bitrates and AQ sheds the TOP layer under the bot's own congestion (`videocall-aq`) | 🟦 DESIGN COMPLETE — DEPLOY-GATED | `uplink_budget_kbps` = Σ ideal over active layers; `cap_layers_to_budget` scales the sum to fit (floors preserved); top-layer drop is the AQ controller's job. Host unit tests in `videocall-aq/src/constants.rs` green; bot-side congestion capture deploy-gated. Run **twice: WT, WS.** |
| V22 | Relay layer-filter correctness — a bot emitting `LayerPreferencePacket{desired_layer:0}` receives ONLY layer-0 `video_bytes` (per-source `inbound_stats`) while a no-preference bot keeps the full ladder | 🟦 DESIGN COMPLETE — DEPLOY-GATED | `layer_preference_sender.rs` (`--pin-layer 0`) emits a base-only preference per discovered source; 11 unit tests green. The per-source `inbound_stats` divergence (pinned vs. no-preference bot) is DEPLOY-GATED on a per-receiver-simulcast relay (#1079+#1082). Run **twice: WT, WS.** |

### #988 viewport-fidelity validation detail

**V17 — bot `media_kind` enables relay viewport filtering.**
The bot now stamps the cleartext `PacketWrapper.media_kind` exactly like a real
client: `VIDEO` on both the VP9 (`video_producer.rs`) and costume
(`video_producer.rs`) paths, and `AUDIO` on the Opus path
(`audio_producer.rs`). Before this, bot packets carried
`MEDIA_KIND_UNSPECIFIED` (0), which the relay's #988 filter treats as fail-open
— so bot VIDEO was *never* filtered for any receiver and the load test reported
more relay headroom than a real, filtered fleet would leave.

How to verify (needs a #988-enabled relay):
1. Run a fleet where at least one receiver (a browser or a `viewport_visible_count`-configured bot) renders a strict subset of peers, excluding bot `X`.
2. On the relay, watch `relay_viewport_filtered_total{room=…}` (the dedicated counter at `chat_server.rs`). It must increment for bot `X`'s VIDEO toward that receiver — proving the relay now recognizes bot VIDEO as filterable.
3. Negative control: with all packets at `media_kind=0` (old binary), the counter stays flat. Confirms the discriminator, not some other change, drives the filtering.
Note: AUDIO is intentionally never filtered, so `media_kind=AUDIO` should *not* cause any drop — only ensure it doesn't regress audio delivery.

**V18 — bot `VIEWPORT` reduces measured inbound `video_bytes` end-to-end.**
With `viewport_visible_count: N` set in the bot config, each bot emits a
`VIEWPORT` packet listing only the first `N` source `session_id`s it has
discovered (ascending order → deterministic across runs). A #988 relay then
stops forwarding VIDEO from the hidden sources to that bot. The visible-peer
choice lives in `viewport_sender.rs::compute_visible`; the source `session_id`
is read from the relay-stamped `PacketWrapper.session_id` fed in via
`inbound_stats.rs::record_packet`.

How to verify (needs a #988-enabled relay):
1. Run e.g. 10 bots with `viewport_visible_count: 3`. Each bot should render 3 of the other 9 peers.
2. Read the bot's own `inbound_stats` RX-STATS line (the 10s `video=… KB` field in `inbound_stats.rs::report`) — per-source `video_bytes` for the 6 *hidden* sources should fall to ~0 after the VIEWPORT is sent, while the 3 visible sources keep flowing. AUDIO for all 9 stays unaffected.
3. Compare against a `viewport_visible_count: null` (legacy) control run: every source's `video_bytes` should remain non-zero (relay fails open, forwards all VIDEO).
4. Cross-check `relay_viewport_filtered_total` increments on the relay in lockstep with the bot's `video_bytes` drop.

**V19 — bot re-asserts its `VIEWPORT` after a subscription loss.**
The relay drops a receiver's viewport subscription on disconnect; on reconnect /
re-election it allocates a fresh empty viewport (fail-open → all VIDEO flows
again). The browser client recovers by re-sending its viewport on the
`Connected` state edge (`video_call_client.rs::reset_for_reconnect`). The bot has
no equivalent connection-state event, and `InboundStats::reset()` preserves the
`ViewportSender` (take/restore) across its 10s diagnostic window — so the
sender's `known_sources` / `last_sent` / `has_sent` survive, and the
change-driven `on_source_seen` path would NOT re-emit (the sources are already
known). Without a re-assert the bot would silently receive all VIDEO again for
the rest of the run, masking the very saving #988 measures.

`ViewportSender::resend_on_reconnect()` re-sends the CURRENT visible subset
unconditionally (the relay's copy is stale even though the local set is
unchanged). It is invoked from `InboundStats::reset()` after the sender is
restored. It is a no-op when legacy (`visible_count == None`), when no viewport
was ever established (`has_sent == false` → first-connect never double-sends),
or when the visible subset is empty; and it is rate-limited
(`MIN_RESEND_INTERVAL = 5s`) so the 10s reset cadence cannot spam identical
packets. Because the trigger is periodic rather than edge-driven, the re-assert
heals ANY subscription loss (reconnect, re-election, relay idle-timeout), not
just an in-process reconnect — the bot currently has no in-process reconnect
loop, so this is the robust forward-looking hook.

How to verify (needs a #988-enabled relay):
1. Run a fleet with `viewport_visible_count: N` and establish steady-state filtering (per V18: hidden sources' `video_bytes` ≈ 0).
2. Force a viewport-subscription loss for one bot (restart the relay pod it is pinned to, or trigger a re-election) without restarting the bot process.
3. Confirm the hidden sources' `video_bytes` briefly rises (relay re-allocated an empty fail-open viewport) then falls back to ≈ 0 within one 10s reset window as the bot re-asserts; `relay_viewport_filtered_total` resumes incrementing.
4. Confirm the bot log shows `Sent VIEWPORT (reconnect) ...` and `viewports_sent` increments on the re-assert.

### #988 prereq rows V17–V19 — status note (#1083-A2)

V17–V19 cover the bot's VIEWPORT path through the **same relay viewport/layer
filter** that the new V22 layer-preference row exercises (#988 viewport-bot
fidelity). They were asked about as a "clear if stale/coverable now" item.

**They are NOT cleared.** All three are CODE-COMPLETE with green unit tests, but
their pass criteria are end-to-end measurements (`video_bytes` drop,
`relay_viewport_filtered_total` increment, recovery after subscription loss) that
require a **live #988-enabled relay** — exactly the same cluster dependency as
V22. There is no in-process harness that stands up the relay's viewport-filter
forwarding path, so they cannot be verified here. Honest status: deploy-gated,
same as the new simulcast rows; do not mark them PROVEN until a cluster run
captures the divergence.

### #1083-A2 per-receiver simulcast — bot-side validation detail

This increment lands the bot **publisher-side + relay-filter validation** for
per-receiver simulcast (#989 / #1079 / #1082). The bot is a publisher with **no
receiver chooser** (no `videocall-client` dependency, no per-tile layer-selection
UI), so it cannot drive the real dynamic chooser — but it can validate the two
halves it participates in: its own multi-layer publish, its own uplink-budget /
AQ behaviour, and the relay's layer filter via a synthetic, fixed preference.

**V20 — bot emits a multi-layer simulcast ladder (publish side).**
With `experimentalSimulcastMaxLayers` raised on the publisher config,
`videocall-aq` builds an `n`-layer ladder (`controller.rs::set_simulcast_layers`)
and the bot stamps the cleartext `PacketWrapper.simulcast_layer_id` per layer so
the relay can layer-filter without decrypting the inner MediaPacket.

How to verify (needs #1079+#1082 deployed):
1. Raise `experimentalSimulcastMaxLayers` to ≥ 2 for the publishing bots.
2. On a receiver with no layer preference, confirm multiple distinct
   `simulcast_layer_id` values arrive per source (the full ladder).
3. Negative control: with `experimentalSimulcastMaxLayers = 1`, only layer 0
   should ever appear.
4. **Run twice: once with `wt_ratio=1.0` (WT) and once `wt_ratio=0.0` (WS)** —
   the layer stamping rides the same outbound frame path for both transports and
   must not regress on either.

**V21 — `uplink_budget_kbps` caps the SUM and AQ sheds the TOP layer.**
`uplink_budget_kbps(tiers, active)` is the sum of the active layers' ideal
bitrates; `cap_layers_to_budget` proportionally scales the per-layer targets down
to fit that budget while never dropping any layer below its tier floor. Dropping
a layer entirely (shedding the TOP layer) is the AQ controller's job when even
the floors no longer fit — the cap function deliberately does NOT shed. Both are
pure and covered by host unit tests in `videocall-aq/src/constants.rs`.

How to verify (needs #1079+#1082 deployed):
1. Run a publishing bot with `experimentalSimulcastMaxLayers ≥ 3` and impair its
   uplink (e.g. `--impair-name X=congested_wifi`, or an inline `uplink_kbps`
   block) below the 3-layer budget.
2. Confirm the SUM of the bot's per-layer `encoder_target_bitrate_kbps` tracks
   the configured uplink budget (cap engaged), not the unconstrained 3-layer
   ideal.
3. Squeeze the uplink further (below the floors of all active layers) and confirm
   the AQ sheds the TOP layer (active layer count drops; the highest
   `simulcast_layer_id` stops being emitted) while the base layer keeps flowing.
4. **Run twice: WT and WS.**

**V22 — relay layer-filter correctness (the key #1083-A2 row).**
A bot launched with `--pin-layer 0` (or `BOT_PIN_LAYER=0`) emits a
`LayerPreferencePacket` whose every `Entry` has `desired_layer = 0` (BASE LAYER
ONLY) for each discovered source `session_id`, exactly like a browser receiver
that selected the lowest tier. The selection is inbound-driven via
`layer_preference_sender.rs::on_source_seen` (fed the relay-stamped
`PacketWrapper.session_id` from `inbound_stats.rs`), change-suppressed, and
re-asserted on the periodic reset hook (the relay drops the recorded preference
on disconnect — same fail-open subscription-loss concern as VIEWPORT). Default is
OFF: with no `--pin-layer` the bot never emits and the relay forwards the full
ladder (fail-open), so existing bot behaviour is unchanged.

How to verify (needs #1079+#1082 deployed):
1. Run two receiver bots against a meeting whose publishers emit ≥ 2 simulcast
   layers (V20): bot `P` with `--pin-layer 0` and bot `Q` with no preference.
2. Read each bot's own `inbound_stats` per-source RX line. For bot `P`, every
   source's `video_bytes` should fall to the **base-layer-only** rate (upgraded
   layers dropped by the relay). For bot `Q`, every source's `video_bytes` should
   stay at the **full-ladder** rate.
3. Cross-check the relay's per-receiver layer-drop counter increments for bot
   `P`'s sources and stays flat for bot `Q`.
4. Confirm `P`'s log shows `Sent LAYER_PREFERENCE (...) pinning N source(s) to
   layer 0 (Video)` and `preferences_sent` increments; after a forced
   subscription loss, confirm the re-assert (`Sent LAYER_PREFERENCE
   (reconnect) ...`) restores the filter within one reset window.
5. **Run twice: WT and WS** — the LAYER_PREFERENCE control packet rides the bot's
   existing transport (whichever it is configured with), so both paths must show
   the same divergence.

> **Deploy gating (be honest about what landed):** the CODE for V20–V22
> (`layer_preference_sender` + wiring + these rows) lands now and is unit-tested.
> The actual CLUSTER RUNS that capture the `inbound_stats` divergence and the
> per-layer bitrate behaviour are **DEPLOY-GATED on #1079 + #1082 being merged
> and deployed** to a relay that implements per-receiver simulcast. Row results
> (✅/🔻 numbers, Prometheus captures) get filled in on that cluster run — they
> are intentionally left blank here rather than fabricated.

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
- [ ] **V17 — bot `media_kind` enables relay filtering.** On a #988-enabled relay, run a fleet with at least one receiver rendering a strict subset, and confirm `relay_viewport_filtered_total` increments for a bot whose VIDEO is off-screen. Negative control with the old (media_kind=0) binary keeps the counter flat. Code complete; not yet run against a live relay.
- [ ] **V18 — bot `VIEWPORT` drops inbound `video_bytes`.** Run 10 bots with `viewport_visible_count: 3`; confirm each bot's `inbound_stats` per-source `video_bytes` falls to ~0 for the 6 hidden sources while the 3 visible sources keep flowing, and AUDIO is unaffected. Compare against a `viewport_visible_count: null` control (all sources keep flowing). Code + unit tests complete; E2E `video_bytes` drop not yet measured.

### Per-receiver simulcast (#1083-A2) — DEPLOY-GATED on #1079 + #1082

- [ ] **V20 — bot multi-layer publish.** Raise `experimentalSimulcastMaxLayers ≥ 2` on publishing bots; confirm multiple distinct `simulcast_layer_id` values arrive per source on a no-preference receiver. Negative control at `=1` shows only layer 0. Run twice (WT, WS).
- [ ] **V21 — uplink budget caps SUM / AQ sheds TOP layer.** Impair a 3-layer publisher's uplink below the 3-layer budget; confirm the SUM of per-layer `encoder_target_bitrate_kbps` tracks the budget (cap engaged). Squeeze below all floors; confirm the AQ drops the top layer while base keeps flowing. Run twice (WT, WS).
- [ ] **V22 — relay layer-filter correctness.** Two receiver bots vs. a ≥2-layer publisher fleet: bot `P` `--pin-layer 0`, bot `Q` no preference. Confirm `P`'s per-source `inbound_stats` `video_bytes` falls to the base-only rate while `Q`'s stays at the full-ladder rate; relay per-receiver layer-drop counter increments for `P`, flat for `Q`. Confirm `P`'s re-assert restores the filter after a forced subscription loss. Run twice (WT, WS). Code + 11 `layer_preference_sender` unit tests complete; E2E divergence deploy-gated.

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

# Per-receiver simulcast layer-filter validation (#1083-A2; needs #1079+#1082 deployed).
# Bot P pins every source to layer 0 (base only); bot Q (separate run/config) keeps
# the full ladder. Compare per-source video_bytes in each bot's inbound_stats RX line.
RUST_LOG=info ./run.sh config-room1.yaml --users 10 --pin-layer 0
# Equivalent via env (e.g. per-pod in a fleet): BOT_PIN_LAYER=0 BOT_PIN_LAYER_KIND=video

# Prometheus: sanity-check a metric exists at all
curl -sk "$PROM/api/v1/query" --data-urlencode 'query=max_over_time(videocall_adaptive_video_tier[7d])'

# Prometheus: peer-perceived quality of target bot
curl -sk "$PROM/api/v1/query" \
  --data-urlencode 'query=avg_over_time(videocall_call_quality_score{meeting_id="1",to_peer="alice"}[180s] @ <UTC_EPOCH>)'

# Convert local time to UTC epoch for Prometheus
date -u -d '2026-05-06T14:57:00 EDT' +%s
```

## Related files

- `bot/src/netsim.rs` — thin `BotNetSimShim` wrapper around the shared `videocall-netsim` crate (adds Prometheus metric hooks)
- `videocall-netsim/src/shim.rs` — shim core (token bucket, drop, jitter, delay)
- `videocall-netsim/src/profiles.rs` — preset definitions
- `bot/src/aq_controller.rs` — BotAq wrapper around videocall-aq
- `bot/src/health_reporter.rs` — tier fields populated here (see V13)
- `bot/src/viewport_sender.rs` — VIEWPORT emitter (#988; V17–V19)
- `bot/src/layer_preference_sender.rs` — LAYER_PREFERENCE emitter / `--pin-layer` mode (#1083-A2; V22)
- `bot/tests/aq_degradation.rs` — deterministic in-process integration test
- `videocall-aq/src/constants.rs` — `uplink_budget_kbps` / `cap_layers_to_budget` + their host unit tests (V21)
- `videocall-aq/src/controller.rs` — `set_simulcast_layers` ladder (V20)
- `actix-api/src/actors/chat_server.rs` — relay records the per-receiver layer preference subject-authoritatively (V22)
- `videocall-aq/` — shared AQ state machine (also used by browser)
