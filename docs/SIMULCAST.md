# Simulcast — what we send, what the server forwards, and where the saving comes from

**Audience:** anyone reasoning about video quality, bandwidth, or layer selection.
**Purpose:** stop re-deriving these rules from source.

Every claim here cites the symbol that delivers it. A number is tagged **MEASURED** (observed on a
running system) or **DERIVED** (computed from constants or from another measurement).

**Terminology:** "video layer N" / "screen layer N" / "audio layer N" means **wire id N** — the
`simulcast_layer_id` stamped on the packet — always prefixed with its media kind, because the three
ladders differ (see §0.1). The physical encode behind a wire id is a *tier*: `low`, `standard`, `hd`.
On a ceiling-3 publisher wire id and tier index coincide; on a ceiling-2 publisher they do not (§1.4).

---

## 0. The word is *simulcast*, not multiplexing, and not SVC

| term | meaning | ours? |
|---|---|---|
| **Simulcast** | Send **N complete, independent** encodes of the same source at once. Each is standalone and decodable by itself. | ✅ **this is what we do** |
| **SVC** (scalable video coding) | Nested layers: the enhancement layer is undecodable without the base. | ❌ **not us** |
| **Multiplexing** | Combining several *different* signals onto one channel. | ❌ different concept |

Because our encodes are independent, **dropping video
layer 1 does not harm video layer 2** — a receiver pulling video layer 2 needs nothing else. Under SVC
that would not hold. The relay states it at `chat_server.rs`'s forwarding filter:

> *"EXACT-MATCH, not cumulative: a recorded preference of layer N means 'forward ONLY layer N from
> this source', NOT 'layers 0..=N'. These are independent simulcast encodes, not nested SVC layers."*

### 0.1 The three ladders are not the same depth

| media kind | publish depth | receive depth | notes |
|---|---|---|---|
| **video** (camera) | up to 3 (a *ceiling* — see §1.3) | 3 | the ladder in §1 |
| **screen** | **1** (`SCREEN_SIMULCAST_MAX_LAYERS = 1`, `videocall-aq/src/constants.rs`) | **1** (`max_layers_for_kind(Screen)` reads the same constant) | §1's ladder does **not** apply — screen is single-layer on the wire and in the chooser |
| **audio** | **1** (`audio_published_layer_count()` returns `1`, `dioxus-ui/src/constants.rs`, #2279) | 3 (`max_layers_for_kind(Audio)`, #2735) | the sole audio encode is stamped `simulcast_layer_id == 0` at the TOP bitrate, walked down by AQ tier, **not** by layer selection |

So the layer filter in §2 (Rules 1–3) applies to all three media kinds — it keys on
`(source, media_kind)` — while the viewport filter and the downlink shed are camera-video only. The §1
ladder describes **video only**.

---

## 1. What a publisher sends

### 1.1 The nominal video ladder — `SIMULCAST_VIDEO_LAYERS` (`videocall-aq/src/constants.rs`)

| video layer | label | max dimensions | target fps | ideal kbps |
|---|---|---|---|---|
| **0** | `low` | 320 x 180 | 7 | **120** |
| **1** | `standard` | 640 x 360 | 15 | **350** |
| **2** | `hd` | 1280 x 720 | 30 | **1500** |

Full ladder ≈ **1,970 kbps** (DERIVED: 120 + 350 + 1500). `SIMULCAST_MAX_LAYERS = 3`.

### 1.2 Dimensions are a BOUNDING BOX and are never upscaled

A video layer's `max_width`/`max_height` bound the output; they do not define it. A 640x480 4:3 webcam
on a ceiling-3 publisher produces `240x180 / 480x360 / 640x480`, not the nominal 16:9 boxes; on a
ceiling-2 publisher it produces `240x180 / 640x480` (§1.4).

**Consequence: you cannot identify a video layer from an observed resolution without knowing the source
geometry and the publisher's ceiling.** On a ceiling-3 publisher a 640-wide frame is video layer 2 on a
4:3 source and video layer 1 on a 16:9 720p source; on a ceiling-2 4:3 publisher it is wire layer 1.
Bitrates are nominal per layer; resolutions are source-bounded. Never read one as the other.

### 1.3 The core-count table is a CEILING, not an operating point

`max_simulcast_layers(cores, platform)` in `dioxus-ui/src/components/capability_check.rs` returns a
**ceiling**:

| condition | ceiling (max video layers) |
|---|---|
| `cores < 6`, unknown core count, or older Intel Mac | **1** |
| `6 <= cores < 10`, and not the row above | **2** |
| `cores >= 10`, and not the first row | **3** |

(`MIN_CORES_FOR_MULTILAYER = 6`, `CORES_FOR_3_LAYERS = 10`.)

**A 10-core machine does not publish 3 video layers by virtue of its core count.** The *operating
point* starts at **1 video layer for everyone** and is ramped up at runtime by the `videocall-aq`
controller (#1141). The effective ceiling is `min(this, experimentalSimulcastMaxLayers)` (code default
3, #1082; the committed dev/E2E `dioxus-ui/scripts/config.js` sets it to **1**, so the local e2e stack
is single-layer unless a spec calls `enableSimulcastFlag`). So "how many
video layers is this publisher actually sending right now" is a runtime question answered only by
observing the wire or `videocall_encoder_layer_output_fps` (§5), never by the core count.

### 1.4 Wire layer ids are DENSE; physical tiers follow the CEILING

Three numbers are in play for one publisher: the **ceiling** (§1.3), the **active count** (how many
layers are being encoded right now), and the **wire id** on each packet.

- **Wire `simulcast_layer_id`** is the **dense encoder index** — the camera encoder builds one
  `VideoEncoder` per built layer, over `enumerate()`, and stamps `simulcast_layer_id = layer_idx as
  u32` (`videocall-client/src/encode/camera_encoder.rs`, the `build_layer` closure + the
  `transform_video_chunk` call). So a publisher with 2 active layers stamps wire ids **0 and 1** —
  **NOT 0 and 2.**
- **Physical geometry** for each wire index is chosen by the **ceiling**, not by the active count.
  The encode loop passes `n_layers = effective_layer_count()` (the clamped ceiling) to
  `simulcast_layer_encode_params`, which forwards it as `layer_count` to
  `camera_layer_encode_box(src_w, src_h, layer_index, layer_count)`
  (`videocall-aq/src/aspect.rs`), which picks the tier table for that ceiling —
  `simulcast_layers(layer_count)`, built over `spaced_ladder_positions` (both in
  `videocall-aq/src/constants.rs`; the latter is private). A ceiling of 2 anchors the base and the top
  (`[0, 2]`) and skips the `standard` middle tier.
- **The active count selects the lowest N of those ceiling tiers.** `encoders_to_build(active,
  ceiling)` returns `min(max(active, 1), ceiling)` and the loop builds indices `0..that`; the AQ ramp
  (#1141) adds and sheds from the top.

```
ceiling 3, 3 active -> wire ids [0, 1, 2]  physical [low, standard, hd]
ceiling 3, 2 active -> wire ids [0, 1   ]  physical [low, standard    ]   <- hd not yet ramped / shed
ceiling 3, 1 active -> wire ids [0      ]  physical [low              ]
ceiling 2, 2 active -> wire ids [0, 1   ]  physical [low,           hd]   <- standard tier does not exist
ceiling 2, 1 active -> wire ids [0      ]  physical [low              ]
ceiling 1           -> wire ids [0      ]  physical [single stream, AQ tier box — not a ladder tier]
```

A ceiling of 1 takes the single-stream path (`simulcast = n_layers > 1` in the encode loop): there is
no ladder box. The stream starts at the native capture resolution and is fitted into the current AQ
tier's box (`local_tier_max_*`) as the tier moves.

**So "2 layers on the wire" is ambiguous until you know the ceiling.** A ceiling-3 publisher mid-ramp
sends `[low, standard]`; a ceiling-2 publisher at its ceiling sends `[low, hd]` with a hole in the
middle of its quality range. Observed in the field (MEASURED, hcl-daily `ch_lab`, 2026-09-16): an
8-core (ceiling-2) publisher sent `240x180@8fps` then jumped straight to `640x480@23fps`, while every
10+-core publisher in the same call sent `240x180 / 480x360 / 640x480`.

**Wire ids are per-publisher, not global.** Wire id 1 is the `standard` tier on a ceiling-3 publisher
and the `hd` tier on a ceiling-2 publisher. **Do not assume "video layer 1" means the same picture from
two different people.**

---

## 2. What the server forwards

Four filters live in the free function `handle_msg` (`actix-api/src/actors/chat_server.rs`), applied
to a media packet **in this order**. The first two can drop a base-layer (`simulcast_layer_id == 0`)
packet; the last two exempt it:

1. **Observer allowlist** — an observer (waiting-room) session receives **only** `MEETING` and
   `SESSION_ASSIGNED`; **all media is dropped**, base layer included. JWT-claim-bound, fail-closed.
2. **Viewport filter (#988)** — VIDEO-only. Drops **every** video layer, base included, from a source
   the receiver's viewport does not want. Default-**ON** (`viewport_filter_enabled()` /
   `resolve_viewport_filter_enabled` default `true` in `actix-api/src/constants.rs`). An empty viewport
   set or an unparseable source fails **open** (forward).
3. **Layer filter (#989)** — exact-match per `(source, media_kind)`. Exempts `simulcast_layer_id == 0`.
4. **Downlink shed (#1219)** — while the receiver's downlink is congested, drops non-base **camera
   video** only. Exempts base layer, screen, and audio.

A packet that survives all four is handed to the session actor with `try_send`; a `Full` mailbox
drops it too, base layer and audio included. That is a burst absorber, not a filter. It is recorded in
`relay_packet_drops_total`, and by then `relay_layer_forwarded_by_layer_total` has already counted the
packet as forwarded (§5).

The three rules below describe filter 3, the layer filter (Rule 1 also notes the shed's separate base
exemption).

### Rule 1 — the layer filter never drops the base layer, regardless of PREFERENCE

```rust
if is_layer_filterable && pw.simulcast_layer_id != 0 && layer_prefs.has_any() {
//                        ^^^^^^^^^^^^^^^^^^^^^^^^^^
```

This gate exempts the base layer from the **layer filter** (#989); `downlink_shed_candidate` carries its
own `simulcast_layer_id != 0` check for the **downlink shed** (#1219). Neither drops a base-layer
packet, *no matter what layer the receiver requested*.

**This is NOT "layer 0 is always forwarded."** A base-layer video packet is still dropped by the
observer allowlist and by the viewport filter. `test_handle_msg_layer_zero_always_forwarded` pins only
the narrower claim: it passes `observer = false`, `DesiredStreams::default()` and `never_epoch()`, which
neutralizes filters 1, 2 and 4.

### Rule 2 — selection is EXACT-MATCH, not cumulative

A receiver asking for video layer 1 gets **the base layer (survives the preference filter) + video
layer 1**. It does not get "layers 0 through 1" as a stack, and it does not get a downgrade if video
layer 1 is missing — a mismatched layer is **dropped**. A receiver whose decode guard sits on a layer
that is not arriving skips every packet and **freezes on its last-good frame** (`videocall-aq/src/constants.rs`:
*"NOTHING decodable and the tile FREEZES on its last-good frame; it does NOT fall back"*). For camera
video the freeze is bounded on the client: once no layer at or above the selected one has arrived
within `LAYER_AVAILABILITY_WINDOW_MS` (4 s), the next lower-layer packet collapses the guard onto the
highest layer still arriving (`collapse_video_guard_to_available`, `peer_decode_manager.rs`, #2251). The relay
does not validate availability — the `AVAILABILITY NOT VALIDATED` note sits on the forwarding path in
`chat_server.rs`, next to the predicate quoted under Rule 3.

### Rule 3 — no recorded preference for a `(source, media_kind)` means "forward it"

The forwarding fail-open predicate is in the layer filter itself:

```rust
st.layers
    .get(&(src, kind_key))
    .is_some_and(|&want| want != pw.simulcast_layer_id)
```

Drop **iff** there is a recorded preference for this `(source, media_kind)` **and** it selects a
different layer. No entry → `is_some_and` is `false` → **forward** (fail-open). A poisoned lock or an
unparseable source also fails open.

(Do **not** confuse this with `compute_max_requested_layer` — that is the *publisher-facing* LAYER_HINT
union (#1108), which tells a publisher which layers every receiver has stopped wanting so it can stop
*encoding* them. It is not on the per-packet forwarding path.)

The preference message **replaces** the whole map rather than merging into it —
`try_intercept_layer_preference`: *"Overwrite (not merge): the latest LAYER_PREFERENCE is the full
current per-source layer map."* A message arriving within `LAYER_PREFERENCE_MIN_UPDATE_INTERVAL` (200 ms) of
the last accepted one is rate-limited and the previous map stays in force.

**So a publisher omitted from a non-empty message fails open exactly as completely as one omitted from
an empty message** (see §5, trap 1).

---

## 3. What a receiver can and cannot ask for

Each monitor tick builds a receiver's request per publisher in `PeerDecodeManager::tick_layer_choosers`
(`videocall-client/src/decode/peer_decode_manager.rs`), gated by `LidDwell::settle`. The read-only seed
path — `Peer::collect_desired_preferences` (private), exposed as
`PeerDecodeManager::current_desired_preferences` — reproduces the same decision with
`LidDwell::peek_settle` (`videocall-client/src/decode/layer_chooser.rs`):

```rust
pub fn peek_settle(&self, t: LidTick) -> Option<u32> {
    if self.surviving_hold(t).is_none() && t.value >= t.observed {
        return None;
    }
    Some(t.value)
}
```

`t.value` is the layer wanted; `t.observed` is `LayerAvailability::highest_available(now_ms)` — the
highest layer seen arriving within `LAYER_AVAILABILITY_WINDOW_MS = 4000`. A `None` return omits the
entry, which by Rule 3 means "send everything."

**A receiver can ask for a LOWER layer. It cannot *start* a request for the HIGHEST currently
observed.** "I want the top I can currently see" and "I have no opinion" are the same message: silence.
(A live `LidDwell` hold, #2630, keeps advertising a layer after the observation drops to it — but only a
layer first requested while something above it was arriving; `surviving_hold`.) This is a known
expressive gap, tracked as **#2200** — *"the control packet exists to subtract, never to request."*

### Availability is learned empirically

The relay never advertises which layers a publisher produces. A receiver learns it only by watching
what arrives inside a 4-second window. `highest_available` can also exclude a **quarantined** layer
(#2328), but in production only `screen_layer_availability` is ever quarantined
(`peer_decode_manager.rs`, keyframe starvation) — for camera video the observation is raw arrivals.

**The periods when a receiver sends nothing are currently the only way it re-discovers that a
publisher's ladder grew** (#2200).

---

## 4. Where the saving actually comes from

Figures are DERIVED from hcl-daily `ch_lab`, 2026-09-16, 7 participants (see the note under the
table). Assume a **3-layer publisher** unless stated; `observed = 2`.

| the receiver wants | can it say so? | server drops | saving (of the ~1,970 kbps ladder) |
|---|---|---|---|
| **video layer 1** (small tile) | ✅ `1 < 2` | video layer 2 | **≈ 1,130 kbps** (≈57% of the nominal ladder) — DERIVED, see note below |
| **video layer 2** (large tile) | ❌ `2 >= 2` (highest *currently observed*) | *(would be video layer 1)* | ≈ 300 kbps (≈15%) — DERIVED |
| **video layer 0** | ✅ `0 < 2` | video layers 1 and 2 | most of the ladder |
| **video layer 0** from a **ceiling-2** publisher (`observed = 1`; tile ≤ 198 device px, see below) | ✅ `0 < 1` | wire layer **1**, which on a ceiling-2 publisher is the `hd`-box encode (§1.4) | the top encode |
| **video layer 1** from a **ceiling-2** publisher (any larger tile) | ❌ `1 >= 1` | *(nothing — base is exempt anyway)* | **0** |

The last two rows are the 2-layer case. A publisher with 2 active layers stamps wire ids **0 and 1**
(§1.4), so a receiver observes `highest = 1`. The size lid picks video layer 0 only for a tile at most
198 device px tall — `size_cap_layer` fits the tile against the 2-layer `[180, 720]` boxes with
`SIZE_CAP_MARGIN = 0.10` (`layer_chooser.rs`) — and then the relay drops every wire-1 packet from that
publisher (`want == 0 != 1`). Any taller tile lids to layer 1 — unless the chooser is
congestion-constrained or a user max applies — which equals `observed`, so the receiver sends nothing
and gets both; #2768 found 2 of the 3 receivers it tabulated for a ceiling-2 publisher in that state for
their whole presence (the third was congestion-constrained and asked for `[0]`). Being able to say
"layer 1" would change nothing, because the layer filter never drops the base (Rule 1).

**The expensive layer is the top one.** With 3 layers arriving (`observed = 2`), dropping it already
works for any tile that lids to video layer 0 or 1 (up to 396 device px). With 2 arriving (a ceiling-2
publisher, or a ceiling-3 publisher mid-ramp), only for tiles that lid to video layer 0 (up to 198
device px). The receiver's arithmetic keys on `observed`, not on the publisher's ceiling.

**DERIVED:** ≈1,130 kbps is the difference of two time-weighted per-pair received means from the same
call (≈1,591 kbps un-lidded, ≈461 lidded); ≈300 kbps is carried over from #2768's closing comment,
which does not give its inputs. Neither is a per-layer measurement; the percentages divide them by the
nominal 1,970 kbps ladder. No metric reports bytes-delivered-vs-bytes-decoded — see §5.

---

## 5. Measuring it

### Traps

1. **Do NOT measure "how often was the preference message empty."** By Rule 3, fail-open is
   **per publisher**. Map-emptiness equals per-publisher fail-open only when exactly one publisher is
   being trimmed. Measure per (receiver, publisher) pair.
2. **Separate "the request lapsed" from "no request was ever sent."** They are different defects
   (#2630 vs #2200) and summing them hides whether a fix worked.
3. **Gate on the publisher's camera being on** (`videocall_peer_video_enabled`), or a camera-off
   publisher counts as "failing open" with no video flowing.
4. **Deduplicate console logs by `.seq` before counting anything.** Chunk `00001` is re-uploaded as the
   prefix of chunk `00002`; every count over a session's first few hundred lines otherwise
   double-counts. `parse_meeting_console_logs.sh` does **not** do this for you.
5. **`relay_layer_forwarded_by_layer_total` counts PACKETS, not bytes**, and a base-layer packet and a
   top-layer packet are nothing like the same size. It cannot answer "how much bandwidth was wasted."

### What the metrics can and cannot tell you

| metric | what it gives you | limit |
|---|---|---|
| `videocall_video_bitrate_kbps` | per-pair received rate | post-filter and post-drop — measures what survived, not what was requested |
| `videocall_received_layer` | the layer a receiver pulled | **only exported while the receiver is actively trimming** — the same gate as §3. Absence is the interesting state and is unrepresentable (#2260) |
| `relay_layer_forwarded_by_layer_total{layer_id}` | packets that survived the viewport and layer filters, per layer | packets, not bytes; room-scoped; incremented **before** the downlink shed and the `try_send`, so it over-counts under congestion |
| `relay_layer_filtered_total` | packets suppressed by preference | room-scoped, no per-pair breakdown |
| `videocall_encoder_layer_output_fps` | **per-video-layer fps per publisher — the way to count how many video layers someone sends** | **camera video only** (`media_kind` label; `metrics_server.rs`), and the per-layer series is **removed** when that layer's fps is unavailable — so **absence of the series ≠ "layer not sent"** |
| `videocall_encoder_active_layers{media_kind="camera"}` | live layer count | ⚠ `media_kind` is `camera`, **not** `video`; a `"video"` filter returns empty |

**Currently unanswerable, tracked as #2769:** how many bytes a receiver downloaded and then discarded.

---

## 6. Related issues

| | |
|---|---|
| **#2200** | Relay never advertises per-publisher ladder availability; fail-open is load-bearing for re-learning. |
| **#2630** | The size lid self-cancelled: honouring a sub-top preference erased the availability evidence that produced it. Fixed by PR #2665 (`LidDwell`). |
| **#2769** | No metric can express per-publisher fail-open or bytes-discarded. |
| **#2768** | *(closed, not planned)* A receiver cannot request the **top** of a 2-layer publisher (`1 >= 1` ⇒ silence). Closed because the base is exempt from the layer filter, so being able to would drop nothing — §4, last row. Requesting the **base** from that publisher already drops wire-1 (§4, row above it). |
| **#1562** | EPIC: simulcast end-to-end validation. |
