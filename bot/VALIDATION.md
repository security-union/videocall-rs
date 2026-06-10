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
| V17 | Bot `media_kind` lets the relay viewport-filter bot VIDEO (no longer optimistic) | ✅ PROVEN (WT) | 2026-06-10 — relay viewport-filtered 5,679 bot VIDEO packets in room `a2vp` (`relay_viewport_filtered_total`); filtering requires the cleartext `media_kind` stamp, so a single filtered packet proves the stamp. WS not separately run (filter is in `chat_server`, after transport ingest — transport-agnostic; V22 proved the adjacent layer filter on BOTH transports). |
| V18 | Bot `VIEWPORT` reduces measured inbound `video_bytes` end-to-end | ✅ PROVEN (WT) | 2026-06-10 — pat (`viewport_visible_count: 1`) vs 2 publishers: visible source delivered 216 kbps; hidden source delivered ZERO video (no `videocall_video_bitrate_kbps{reporter_name="pat",peer_name="bob"}` series ever created); `relay_viewport_set_size`=1, forwarded=18,177 / filtered=5,679. |
| V19 | Bot re-asserts its `VIEWPORT` after a viewport-subscription loss (reconnect / re-election / relay idle) so filtering does not silently lapse | ⏳ PARTIAL | 2026-06-10 — the periodic re-assert is live on cluster: `Sent VIEWPORT (reconnect) rendering 1 of 2 known peer(s)` every 10s reset window, relay `accepted`=23 / `rate_limited`=1 over ~4 min — a dropped subscription is re-asserted within one window by construction. The forced-loss capture itself (relay pod restart mid-run) NOT exercised — too disruptive on the shared daily cluster; run during a maintenance window. |
| V20 | Bot-as-publisher emits a multi-layer simulcast ladder when `experimentalSimulcastMaxLayers` is raised (publish side of per-receiver simulcast #989/#1082) | ✅ PROVEN (WT + WS) | 2026-06-10 — `relay_layer_forwarded_by_layer_total{room="a2sim"}`: WS pod L0=25,845 / L1=5,717 / L2=5,716; WT pod L0=29,011 / L1=5,711 / L2=5,708 — full ladder on both transports. Negative control (`--simulcast-layers 1`, room `a2neg`): ONLY the `layer_id="0"` series exists (9,844 pkts; L1/L2 series never created). |
| V21 | `uplink_budget_kbps` caps the SUM of active-layer bitrates and AQ sheds the TOP layer under the bot's own congestion (`videocall-aq`) | ✅ PROVEN (WT + WS) — shed path | **Bot-side AQ now wired (this branch).** `BotAq::set_simulcast_layers(N)` enables the controller's per-layer paths (full-ladder, shed-only — NOT the browser start-at-base ramp; see code comment); the simulcast producers read `BotAq::simulcast_snapshot` per frame, skip layers ≥ active (top-down shed), and `update_bitrate_kbps` on cap rescale. **Shed trigger = the bot's OWN uplink saturation, measured inside the netsim shim**: the netsim uplink shim records, per packet, the microseconds of delay it imposed *solely* because its token bucket was in deficit (offered byte rate > `uplink_kbps`), exposed as `NetSimShim::bandwidth_wait_us` — a bandwidth-ONLY counter that EXCLUDES latency/jitter/reorder. The AQ tick samples it (`main.rs`), and `BotAq::observe_uplink_saturation` maps a positive per-tick delta to `ENCODER_QUEUE_BACKPRESSURE_HIGH`, arming the controller's existing sustained-shed timer (`controller.rs::backpressure_decision` → `drop_top_layer`). This replaced an earlier `transport_drops_counter`/`try_send`-failure design that was INERT on a real run: the outbound shim spawns a detached delay task per `Admission::Delay`, so `packet_tx` never backs up under bandwidth shaping and the drop counter stayed flat. Measuring saturation at the source (inside `admit`) is immune to that drain. 9 bot unit tests (5 AQ-controller incl. the saturation shed + its flat-counter negative control, 4 producer directive) + 2 end-to-end integration tests (real-shim shed + latency-only negative control) + 4 netsim shim tests (saturation climbs, flat under latency-only/within-budget/passthrough) + the host `cap_layers_to_budget` tests green. **3-layer floors = [200,500,800] kbps** (low/standard/hd). **Cluster capture 2026-06-10 (see the V21 test-run entry): shed fired on BOTH transports** — uplink 300 kbps: SHED 3→2→1 within ~8s of producer start, then a shed/restore equilibrium hunt (WS 12 SHED/11 RESTORE, WT 8/7 over ~5 min); uplink 100 kbps: pinned at active_layers=1, base layer kept flowing end-to-end (relay L1/L2 series flatlined at their first-seconds counts; receiver kept getting alice). Logged active-sum tracked the budget stepwise 2800→1300→400 kbps. SCOPE NOTE (honest): the budget-CAP half (per-layer target rescale via `cap_layers_to_budget`) cannot manifest in the bot — per-layer targets are pinned at tier ideals so the sum equals the budget exactly until a shed changes the active set; the cap function remains host-unit-tested only. The on-cluster proof is the SHED half + stepwise sum tracking. |
| V22 | Relay layer-filter correctness — a bot emitting `LayerPreferencePacket{desired_layer:0}` receives ONLY layer-0 `video_bytes` (per-source `inbound_stats`) while a no-preference bot keeps the full ladder | ✅ PROVEN (WT + WS) | 2026-06-10 — same source, two receivers, 3-min averages: WS — alice's video reached no-pref bob at 373 kbps / 20.3 fps vs pinned pat at 81 kbps / 6.9 fps (4.6×); bob's video 230 vs 49 kbps (4.7×). WT — alice's video 412 vs 72 kbps (5.7×); bob's 229 vs 51 kbps (4.5×). Relay filtered ≈2,130 pkt/min sustained on each transport (`relay_layer_filtered_total` WS=8,164; WT +6,393/3min); `relay_layer_preference_updates_total{outcome="accepted"}` WS=24 / WT=29, `rate_limited`=1 each. pat logged `Sent LAYER_PREFERENCE (change) pinning 2 source(s) to layer 0 (Video)` + 10s `(reconnect)` re-asserts. |

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

**Cleared 2026-06-10** (run details in the `2026-06-10` test-run entry below):
V17 and V18 are PROVEN on the HCL daily cluster (room `a2vp`) — the relay
viewport-filtered bot VIDEO end-to-end and the hidden source's `video_bytes`
dropped to zero at the receiver. V19 is PARTIAL: the periodic re-assert was
captured live (10s `(reconnect)` cadence, relay-accepted), but the forced
subscription-loss sequence (relay pod restart mid-run) was deliberately not
executed on the shared daily cluster; finish it during a maintenance window.

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

> **Bot-side AQ wiring (this branch — #1083 V21).** The deferral to Tony's AQ
> rework (#1115/#1117) is over; those PRs are in PR-staging. `BotAq` now calls
> `EncoderBitrateController::set_simulcast_layers(N)` so the per-layer budget cap
> (`cap_layers_to_budget`) and the top-layer shed path are reachable in the bot.
> **Deliberate divergence from the browser:** the bot uses `set_simulcast_layers`
> (ALL N layers active from frame 1, shed-only), NOT the browser camera path's
> `set_simulcast_ceiling_start_at_base` (start at base, probe up). Rationale: a
> synthetic load generator must publish the full ladder deterministically (V20
> relies on it) and never probe layers UP.
>
> **What actually trips the shed (be precise):** the post-#1115 controller sheds
> when its encoder-queue-backpressure timer (`backpressure_decision` →
> `update_from_backpressure` → `drop_top_layer`) sees depth ≥
> `ENCODER_QUEUE_BACKPRESSURE_HIGH` (3) sustained for
> `ENCODER_BACKPRESSURE_SUSTAIN_MS` (1.5 s). The native bot has no WebCodecs
> encoder, so it cannot report a real queue depth. The HONEST equivalent it DOES
> produce: **uplink saturation measured inside the netsim shim.** When the netsim
> uplink token bucket is in deficit (the offered byte rate exceeds `uplink_kbps`),
> `admit()` returns `Admission::Delay(d)` whose Step-2 (bandwidth) component is
> accumulated into a new lock-free counter, `NetSimShim::bandwidth_wait_us`. That
> counter advances **only** for the bandwidth/token-bucket component — it
> deliberately EXCLUDES the base-latency, jitter, and reorder delay that `admit`
> adds afterward. The AQ tick task samples `shim.bandwidth_wait_us()` each tick
> (`main.rs`) and feeds it to `BotAq::observe_uplink_saturation`; a positive
> per-tick delta maps to depth = HIGH (arming the shed timer), no new
> bandwidth-wait maps to 0 (letting it recover). The controller's own hysteresis
> debounces it, so a single transient deficit does not shed. This is a real
> signal the bot genuinely produces — it is NOT a fabricated trigger.
>
> **Why NOT `transport_drops_counter` (the earlier, INERT design):** the outbound
> shim (`run_outbound_shim`) drains `packet_rx` in a `while let Some = recv().await`
> loop and, for `Admission::Delay`, SPAWNS A DETACHED TASK that sleeps then sends,
> immediately recv'ing the next packet. So `packet_rx` drains at full speed
> regardless of `uplink_kbps`, `packet_tx` (cap 500) never fills, the producers'
> `try_send` never fail, and `transport_drops_counter` stays flat on a real run —
> the shed never armed. (Unit tests that fed the counter directly hid this.)
> Measuring saturation at the source inside `admit()` is immune to the channel
> drain: the bandwidth deficit is recorded when the bucket is consulted, not when
> a downstream channel happens to back up.
>
> **Will a 1500/500 kbps squeeze trip it on a cluster?** Yes. A 3-layer bot
> offers ~2800 kbps of video. At `uplink_kbps=1500` the bucket cannot drain the
> offered load, so `admit` returns `Delay` with a non-zero bandwidth component on
> a sustained basis → `bandwidth_wait_us` climbs every tick → `observe_uplink_
> saturation` reports depth=HIGH across ≥ 2 AQ ticks (≥ 1.5 s sustain) → shed.
> The cap phase (1500) engages the budget cap AND begins a shed; the 500 phase
> (below the 1500 floor-sum) forces shedding down toward base. Grep `BotAq:
> simulcast layer SHED` to see the active-count drop, and confirm `bot_netsim_
> delay_ms{direction="up"}` is non-zero. Because the signal is bandwidth-only, a
> latency-only profile (no `uplink_kbps`, e.g. a 150ms-latency mobile preset)
> leaves `bandwidth_wait_us` flat and NEVER sheds — verified by the
> `latency_only_does_not_trip_uplink_shed` integration test. **Floors: [200, 500,
> 800] kbps.**

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

> **Deploy gating — CLEARED 2026-06-10:** the gate (#1079 + #1082-via-#1086,
> plus #1064/#1060/#1122) was verified in the deployed daily build `87d28f30`
> (`/api/v1/versions` + `git merge-base --is-ancestor`), and the cluster runs
> were executed the same day — see the `2026-06-10` test-run entry below. V20
> and V22 are PROVEN on both transports; V21 turned out to be gated on the bot
> AQ wiring, not on deploy. NOTE: an earlier revision of this note framed the
> V21 blocker as "waiting for #1115/#1117" — those PRs had in fact merged on
> 2026-06-06; the missing piece was always the bot-side wiring, which this
> branch adds (see the V21 row).

## Test runs

### 2026-06-10 (later) — V21 uplink-squeeze shed on HCL daily (bot AQ wiring branch)

**Setup:**
- Binary: built from `feat/1083-bot-simulcast-aq-wiring` (this branch — the V21
  wiring is NOT in the deployed bot; relay side needs nothing new), static
  libvpx, deployed as `jenkins-volt-mx-go-3:~/bot-a2/bot-v21`.
- Topology per run: 1 publisher (`alice`, costume, `--simulcast-layers 3`, manifest
  `network: {uplink_kbps: N}`) + 1 receiver (`pat`, no preference). Four runs:
  WS/WT × phase A (`uplink_kbps: 300`) / phase B (`uplink_kbps: 100`), ~3–5 min
  each, rooms `a2v21wsa`/`a2v21wsb`/`a2v21wta`/`a2v21wtb`.
- **Calibration note:** the ladder's NOMINAL sum is 2800 kbps but the costume
  content's ACTUAL VP9 output is ≈400 kbps full-ladder (static frames compress
  far below target — measured in the earlier V22 runs). The token bucket sees
  actual bytes, so the squeeze phases were calibrated to actual (300/100), not
  nominal (the original 1500/500 design would never saturate).

**Results — shed fired on both transports:**

| Run | Uplink | SHED/RESTORE | Settled state | Relay fwd L0/L1/L2 | pat ← alice |
|---|---|---|---|---|---|
| WS A | 300 kbps | 12 / 11 | hunt 1↔2 layers | 8,654 / 153 / (none) | 208 kbps |
| WS B | 100 kbps | 2 / 1 | pinned at 1 | 9,932 / 67 / 33 | 177 kbps |
| WT A | 300 kbps | 8 / 7 | hunt 1↔2 layers | 10,200 / 494 / 32 | 239 kbps |
| WT B | 100 kbps | 2 / 1 | pinned at 1 | 10,194 / 66 / 32 | 182 kbps |

- First shed lands ≤8 s after producer start (e.g. WT B: `SHED 3 -> 2` at +3 s,
  `2 -> 1` at +6 s). Logged active-sum tracks the budget stepwise
  2800→1300→400 kbps. AQ_STATUS settles at `active_layers=1`,
  `target_bitrate=400`, `video_tier=minimal(7)`.
- L1/L2 relay counters flatline after the first seconds (L2 counts of 32–33 are
  the pre-shed startup burst; the WS-A L2 series was never created because the
  receiver joined post-shed). L0 keeps climbing throughout — base layer always
  flows.
- At 300 kbps (between the 1-layer and 2-layer ACTUAL rates) the controller
  hunts: sustained-deficit shed alternating with recovery restore on a ~20 s
  cycle. Expected for a cap inside the hysteresis band; a cap clearly below
  (100) pins cleanly at base.
- **Shaping-fidelity caveat (V8/V15 territory, not V21):** the receiver-measured
  delivered rate (177–239 kbps) can EXCEED the configured uplink cap — the
  netsim bucket delays but never drops for bandwidth, and its per-packet delay
  clamps at 5 s, so sustained overload leaks through above the cap. The shed
  trigger is unaffected (it keys on the bucket-deficit counter, not the wire
  rate).
- Audio note: at 100 kbps the audio tier degraded to `emergency(3)` — the
  squeeze pressures the whole uplink, as expected.

### 2026-06-10 — #1083-A2 per-receiver simulcast + #988 viewport fidelity on HCL daily

**Setup:**
- Binary: built from `PR-staging` @ `a1bc0a1f`, static libvpx, deployed to
  `jenkins-volt-mx-go-3:~/bot-a2/`.
- Cluster: HCL daily (`app.videocall.fnxlabs.com`), deployed build `87d28f30`
  (verified to contain #1079, #1086 (#1082), #1064, #1060, #1122 and the
  `layer_preference_sender` commit `48043c14` via `git merge-base --is-ancestor`).
- Topology per run: process 1 = `alice` + `bob` costume publishers
  (`--users 2 --simulcast-layers 3`, ladder L0=640×360@20fps/400kbps,
  L1=960×540@30fps/900kbps, L2=1280×720@30fps/1500kbps); process 2 = `pat`
  (EKG, own single-participant manifest, `--simulcast-layers 1 --pin-layer 0`).
- Rooms: `a2sim` (V20/V22, WS run then WT run), `a2neg` (V20 negative control,
  WT), `a2vp` (V17–V19 viewport run, WT, single-layer publishers,
  `viewport_visible_count: 1`, no pin). Each soak ≈3 min.

**V20 — multi-layer publish (PROVEN, WT + WS).**
`relay_layer_forwarded_by_layer_total{room="a2sim"}`: WS pod L0=25,845 /
L1=5,717 / L2=5,716; WT pod L0=29,011 / L1=5,711 / L2=5,708. Publisher logs:
`Costume simulcast producer started for alice: 3 layers, native=1280x720 ...`.
Negative control (room `a2neg`, `--simulcast-layers 1`, WT): only the
`layer_id="0"` series exists (9,844 packets); L1/L2 series never created.

**V22 — relay layer-filter correctness (PROVEN, WT + WS).**
Same source, two receivers, `avg_over_time(videocall_video_bitrate_kbps[3m])`:

| Source | Receiver (no pref) | Receiver pat (`--pin-layer 0`) | Ratio |
|---|---|---|---|
| alice (WS) | bob: 373 kbps / 20.3 fps | 81 kbps / 6.9 fps | 4.6× |
| bob (WS) | alice: 230 kbps / 32.0 fps | 49 kbps / 10.4 fps | 4.7× |
| alice (WT) | bob: 412 kbps | 72 kbps | 5.7× |
| bob (WT) | alice: 229 kbps | 51 kbps | 4.5× |

Relay-side cross-check: `relay_layer_filtered_total{room="a2sim"}` ≈2,130
pkt/min sustained on each transport (WS measured `rate(...[3m])*60` = 2,135,
cumulative 8,164 at soak end; WT `increase(...[3m])` = 6,393 ≈ 2,131/min) —
pat is the only preference-holder in the room, so attribution is unambiguous
(no-preference receivers never enter the filter gate).
`relay_layer_preference_updates_total{outcome="accepted"}` WS=24 / WT=29;
`rate_limited`=1 on each (DoS guard alive). pat logs: `Sent LAYER_PREFERENCE
(change) pinning 2 source(s) to layer 0 (Video)` on discovery, then a
`(reconnect)` re-assert every 10s reset window. Absolute received rates sit
well below the nominal ladder sum (static costume/EKG content compresses far
below target bitrate) — the per-receiver DIVERGENCE is the assertion, and it
held on both transports with the publisher's encode set unchanged.

**V17/V18 — viewport fidelity (PROVEN, WT) + V19 (PARTIAL).**
Room `a2vp`: pat (`viewport_visible_count: 1`) discovered 2 publishers, sent
`VIEWPORT (change) rendering 1 of 2 known peer(s)`. Visible source (alice)
delivered 216 kbps to pat; hidden source (bob) delivered ZERO video — the
`videocall_video_bitrate_kbps{reporter_name="pat",peer_name="bob"}` series was
never created. Relay: `relay_viewport_set_size`=1, forwarded=18,177,
filtered=5,679, updates accepted=23 / rate_limited=1. V17 follows from any
nonzero filtered count (filtering keys on the bot's cleartext `media_kind`).
V19: periodic `(reconnect)` re-assert captured at the 10s cadence; the forced
relay-side subscription loss (pod restart) was NOT run on the shared cluster.

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
- [ ] **V21 — uplink budget caps SUM / AQ sheds TOP layer.** Bot-side AQ now wired (this branch). Impair a 3-layer publisher's uplink. **Cap phase:** set `uplink_kbps ≈ 1500` (below the 3-layer budget of 2800 = 400+900+1500); confirm the SUM of active per-layer targets tracks the budget (cap engaged — grep `BotAq: simulcast per-layer target rescale` and the controller `AQ_STATUS ... target_bitrate=`). **Shed phase:** squeeze `uplink_kbps ≈ 500` (below the 3-layer floor-sum of 1500 = 200+500+800, so even the floors can't all fit → the AQ must drop the top layer); confirm `BotAq: simulcast layer SHED 3 -> 2 ...` then `... 2 -> 1`, `AQ_STATUS active_layers=` drops, and the highest `simulcast_layer_id` stops appearing while base (id 0) keeps flowing. The shed keys off the netsim uplink shim's bandwidth-saturation counter (`NetSimShim::bandwidth_wait_us`), so also confirm `bot_netsim_delay_ms{direction="up"}` is non-zero during the squeeze; a flat uplink-delay histogram means the offered load fit the cap (lower `uplink_kbps`). **Floors for run design: [200, 500, 800] kbps** (base/mid/top). Run twice (WT, WS).
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
