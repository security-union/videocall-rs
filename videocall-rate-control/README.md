# videocall-rate-control

This crate decides how many bits per second a publisher's video encoder should produce. The viewer measures the path and the publisher decides. Each viewer reports two numbers about the stream it receives: how fast the path carries bits, and how much is queued in front of it. The publisher turns the latest reports into an encoder target, a picture tier and a frame-skip pattern.

It is plain std Rust with no I/O and no clock. Every time is an argument in milliseconds, so it builds for `wasm32-unknown-unknown` and native, and every decision can be replayed in a test.

## The kitchen sink

The example I give people is a kitchen sink. The encoder is the faucet, the network path is the drain, and the backlog is standing water in the basin.

If the faucet runs slower than the drain, the basin stays empty and the water you pour shows up at the bottom right away. If it runs faster, water stands. Every new drop has to wait behind it, and that wait is the video delay people feel in a call.

The viewer sits under the sink. It sees each frame come out of the drain and knows when it was poured, from the frame's timestamp. From that it works out two things. The first is the drain rate (capacity), measured while water is standing, because only then is the drain running flat out. The second is the depth of the water (backlog), the wait the latest frame found. A keyframe is a big bucket poured at once. It makes a brief puddle that its own size explains, so it does not count as standing water.

The publisher has its hand on the faucet. Three moves:

- **Drain.** There is water standing, so turn the faucet down until the basin empties within one feedback cycle, and never below half the drain rate. The faucet is never turned up to drain: if the drain is wide enough to empty the basin at the current flow, the flow stays. With no drain rate measured yet, the current flow is the best guess at it, since it is already too much.
- **Probe.** The basin is empty and the news is fresh, so open the faucet a hair above the drain rate to find out whether the drain got wider. The hair is one frame's worth per feedback cycle, so a wrong guess leaves at most one frame of water.
- **Hold.** The news is old, so pour at the drain rate and wait.

The drain rate only goes up when a higher flow went through with nothing standing. That is why the picture tier follows capacity and not the faucet setting: the probe wiggles the faucet, and the tier must not wiggle with it.

> **For the nerds:** a sink never loses water, and that is where the analogy stops. Our transports are reliable (WebSocket, QUIC streams), so a lost packet shows up as a retransmission, which is delay, which is what we measure. Frames do get dropped whole after they are numbered: the WebSocket path drops when its send buffer is over 1 MB, and the traffic shaper drops above its cap. The viewer sees that as a sequence gap. It holds the frames behind the gap for at most one capacity window (2 s), then gives the gap up. A real drain also never delivers water out of order. WebTransport, with one stream per frame, does, so the viewer puts frames back in sequence before it measures anything.

## How to wire it

### Viewer: one `ViewerEstimator` per remote video stream

For every received video frame, call `on_frame(FrameInfo { .. })` with:

| Field | Source |
|---|---|
| `seq` | `VideoMetadata.sequence` |
| `timestamp_ms` | `MediaPacket.timestamp` (sender clock, any offset is fine) |
| `arrival_ms` | viewer clock when the whole frame arrived, never decreasing |
| `bytes` | the frame's payload size |

Every report period (500 ms today), call `report(now_ms)`. It returns `None` when there is nothing new to say: whenever no frame was released since the last tick (a paused camera, frames held behind a lost one). Then you send nothing, and the publisher ages the report it has and holds. Otherwise put `capacity_kbps()` and `backlog_ms_rounded()` on `DiagnosticsPacket.video_metrics`. When `on_frame` returns `early_report: Some(report)`, send that diagnostics packet at once, outside the tick. At most one goes out per report period.

A report carries a capacity only once a busy period has measured it, that is, once a queue drained at that rate. Before that the delivered rate is a lower bound. An encoder on a still scene sends a fifth of what it may, and "at least 270 kbps" says nothing about a ceiling. So the report says "no ceiling seen", which is `NO_CEILING_KBPS` (`u32::MAX`) in `capacity_kbps`. Same two fields. Zero keeps meaning "no report", which is what older clients send. A queue that has just started can still be reported before its rate is measured: then the report has a backlog and no ceiling, and the publisher drains from the rate it is sending until the measured one arrives. The estimator sends the early report in that state too.

There is no need to call `reset()` when the camera restarts. The estimator spots a new run of the sequence by itself: a sequence number that went back while the timestamp went forward. Call `reset()` only when the stream itself is replaced.

**Requirements on the sender.** A frame the publisher deliberately skips (paused camera, tier frame rate, drain decimation) takes no sequence number. Frames dropped after they were numbered (a full send buffer, the shaper, the relay) are fine: that is exactly what the gap rule is for. And the timestamps of one stream come from one clock that does not jump back when the camera restarts. If it does, the estimator still recovers, after one 2 s window.

### Publisher: one `MultiViewerPublisher`

Configure it with `PublisherConfig { start_bps, max_bps, frame_interval_ms, report_interval_ms, floor_bps }`. The report interval is the viewers' report period. `floor_bps` is the bottom tier's floor, `tiers.last().floor_bps()`. Keep one publisher for the whole call. On a camera restart or a frame-rate change, call `set_frame_interval_ms` or `reconfigure`, which keep the reports, so the target goes on from the remembered capacity instead of jumping back to the cap. Then:

- On every diagnostics packet carrying the two fields: `on_report(viewer_id, now_ms, Report::from_wire(kbps, ms))`. On leave: `remove_viewer(&viewer_id)`.
- On the relay's CONGESTION packet: `on_congestion_signal(now_ms, rtt_ms, from)`. `from` is the congested receiver's viewer id when you can map it, else `None`.
- For every frame handed to the transport: `on_frame_sent(now_ms, rtt_ms)`. That is how a viewer that has reported and then goes quiet gets noticed.
- When the link to the relay drops: `on_link_lost(now_ms, rtt_ms)`, instead of rebuilding the publisher or reconfiguring its start rate. The rate in force is held and climbs back by one probe step per cycle, up to the start rate or the held rate, whichever is higher, where a call without reports would run.
- At every camera frame: `target_bps(now_ms, rtt_ms)`. The caller supplies `rtt_ms`, its round-trip estimate to the viewers (use the largest).
- At every camera frame: `current = next_tier(&tiers, current, now_ms, rtt_ms)`, starting from 0. It goes down at once and up at most one rung per feedback cycle, because a step up only shows in the reports a cycle later. With no report it picks the start rate's tier by itself, so the caller needs no fallback.
- For the stats: `limit(now_ms, rtt_ms)` says what holds the rate below what the reports give, as a `LimitSource` (relay signal, quiet viewer, lost link, or just the climb) and a rate.
- Encoder bitrate: `encoder_bitrate_bps(&tiers[current], target)`.
- Per camera frame: `FrameGate::admit(&tiers[current], frame_timestamp_ms, target)`. Only `Encode` frames go to the encoder and get a sequence number.

One rule covers every way up that nothing measured. The target's ceiling never rises by more than one probe step per feedback cycle when the rise is not backed by a measurement: a viewer with a ceiling leaves, a quiet viewer comes back without one, a cut grows out, the link returns, the cap goes up, or there are no reports at all. Decreases apply at once. A rise a viewer measured is taken at once, because the path already showed it. That means a new measurement: the viewer's first ceiling, or one above its own last. An unchanged ceiling from minutes ago is old news. The lowest ceiling still wins, so one viewer reporting a huge one lifts nothing past the others; alone, it is bounded by the cap and by its own next backlog report. A measurement never undoes a relay cut, which speaks for viewers that do not report. A step up shows in the reports one cycle later, so one step per cycle is as fast as the publisher can learn whether it was safe.

Viewers that never report (older web clients, and the native clients, which send no diagnostics) are covered by the relay signal. A signal halves the ceiling and the rule above brings it back. That is AIMD built from the policy's own numbers. A signal blamed on a viewer whose fresh report has a ceiling is ignored, because its reports already say how much it takes. One whose report has no ceiling has said nothing about a limit, so the signal counts.

A viewer that has reported and then goes quiet while frames keep going out gets the same cut, once per quiet spell, and at most one quiet cut per cycle across all viewers. Viewers whose reports stop together (our own downlink stalls) are one event. One halving stands for one missing report: silence is often not our path at all (a phone in the background, the viewer's own uplink dropping its reports). The trigger is its report going stale (one cycle plus one report period) plus one 2 s capacity window, the longest a healthy viewer stays quiet while it waits on a lost frame. Quiet time only counts while frames go out, so a camera that is off, a viewer that left and one that never reported cut nothing. When it reports again, its report takes over, and the climb back follows the one rule.

When no report has a ceiling, nothing has queued, so nothing pulls the rate down. It climbs from where it was by the same rule, and the first queue brings it back.

The tier table is data you pass in. `default_tiers(max_bps)` gives the measured ladder: 1280x720@30 from 960 kbps, 854x480@30 from 600 kbps, 640x360@30 from 360 kbps and 640x360@15 from 270 kbps, all capped at `max_bps`. When the cap is below 270 kbps, the bottom tier's floor drops to the cap, so a low bitrate setting never turns into endless frame skipping.

### One slow viewer slows everyone

`combine_reports` acts on the lowest capacity and the largest backlog among viewers with a fresh report. With one encoder, that is the only rule that never overruns anybody's path. It also means one viewer on a bad link lowers the picture for the whole meeting, down to a floor: no viewer, honest or hostile, can take the room below the bottom tier at half its frames. Whether to drop that viewer to a lower layer, or ignore it past some point, is an open product decision. The rule lives in that one function so it can be swapped.

## Known limits

The first three are measured in this crate's simulation, and each has an `#[ignore]` test that prints the numbers. A3 and the WebSocket result come from the proof of concept. The rest follow from the design.

- **Jitter seeds 1 and 4.** With 0 to 40 ms of delivery jitter on a 2 Mbps link, the target dips to 841 and 911 kbps against a 936 kbps cap. Seeds 2, 3 and 5 hold.
- **Keyframe on the step.** When a keyframe leaves 0 or +50 ms after the link narrows, the first backlog report comes 424 and 566 ms after the step. The limit is 271 and 287 ms, the run without keyframes plus one frame. At -100 and +100 ms it is on time. The keyframe's own bytes hide the step until it has drained.
- **Two oversized frames in a row.** A 22 KB keyframe followed by a 70 KB delta (VP9 on static content) on a healthy FIFO link reads as congestion: 38 congested frames and 4 backlog reports. Out of order on a shared link the same pair gives 3 and 3. The excuse covers one frame's bytes, not two. The likely fix is on the publisher side, which knows its own per-frame budget.
- **Narrow-phase estimate on recorded runs (A3).** Replaying the 24 recorded browser runs, the capacity in the narrow phase lands inside the 300 to 500 kbps band in 5 of 24, median 492 kbps. That is at the edge of the criterion. The replay data lives in the proof of concept and is not ported here.
- **WebSocket, live (browser runs, not simulated).** 1 to 2 backlog reports per run in the 2 Mbps phase.
- **Other traffic on the same uplink.** The viewer measures what the camera stream gets, not the whole pipe. A screen share or a file upload on the same uplink takes its share, so the camera's capacity ratchets down while it runs. Afterwards it recovers at the probe step, about 5% per feedback cycle.
- **A stalled viewer.** A viewer whose main thread stalls (a slow phone, a busy tab) stamps arrivals late, and that reads as lateness on the path. The publisher drains for a network problem that is really a CPU one.
- **A quiet viewer gets one halving.** The trigger is about 3 s (stale report plus one capacity window). In the simulation a viewer goes quiet behind 250 kbps while 936 kbps goes out: the target halves to 468 kbps, then climbs back to 936 kbps while the viewer is still quiet, and its queue peaks at 21 s. That is the price of not reading every silence as our fault. When the relay queue overflows, its signal does the rest.
- **The relay signal is coarse.** It is one bit and arrives late, after the relay queue (256 messages) has filled. In the simulation a silent viewer behind 300 kbps gets the link fully used (294 kbps delivered), but the target saws between 135 and 720 kbps instead of settling.
