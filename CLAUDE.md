# CLAUDE.md

## Project Overview

`videocall-rs` is a Rust-based video calling platform. The main crates are:

- **videocall-client** - Client library targeting `wasm32-unknown-unknown`.
- **dioxus-ui** - Dioxus-based frontend (the sole UI, uses `videocall-client`)
- **videocall-types** - Shared protobuf types
- **videocall-codecs** - Audio/video codec wrappers

## Media Terminology (use these terms exactly)

Simulcast ladder positions are **layers**, indexed `0=base, 1=mid, 2=high/top`. **Always prefix the media kind** — "video layer 2", "audio layer 0". Never write a bare "layer" or "rung": `rung` is used interchangeably as a synonym in code (`RungKind::Audio`, `ProbeRung`, and `health_packet.proto` uses both words for the same concept), so an unqualified term is ambiguous across audio/video/screen. Existing code identifiers stay as they are — this governs prose, comments, PR bodies, and issue text.

- **Video** publishes 3 layers, nominal `[0] 320x180 ~120kbps | [1] 640x360 ~350kbps | [2] 1280x720 ~1500kbps` (`SIMULCAST_VIDEO_LAYERS`), 1970 kbps total. These are INDEPENDENT simulcast encodes, NOT nested SVC (see `videocall-aq/src/constants.rs`), and the receiver's layer guard is EXACT-MATCH — a mismatched layer is dropped, not downgraded.
- **A layer's dims are a bounding box and are NEVER upscaled**, so an observed resolution does not identify a layer without the source geometry. A 640x480 4:3 source fits to `240x180 / 480x360 / 640x480` (`camera_small_four_three_source_does_not_upscale`), so a 640-wide frame there is layer 2; a 16:9 720p source fits the nominal boxes exactly, so a 640-wide frame is layer 1. Bitrates are nominal per tier; resolutions are source-bounded — never read one as the other.
- **Audio** publishes 3 layers: `[0] 12kbps | [1] 24kbps | [2] 48kbps` (`AUDIO_SIMULCAST_LAYER_KBPS`). Receivers typically settle on the top one, but the chooser starts at `[0]` and sheds downward under downlink constraint (`tick_audio_layer_chooser`, fed the video downlink as a health proxy).
- **Qualify every bitrate and frame count the same way.** An unqualified "1970 kbps" or "1 frame/sec" has already been read as audio when it meant video.
- **`fps` is video-only.** There is no audio fps. Audio's rate analogue is `videocall_neteq_packets_per_sec`; audio health is `videocall_audio_concealment_pct` plus `videocall_neteq_audio_buffer_ms`.
- **"Frames" is NOT video-only** — `AudioFrame` is a real type (`neteq/src/neteq.rs`). Qualify it too.

### Reading the diagnostics — three misleading names

Verify direction and units against the source before concluding anything from these. The first two each produced a wrong conclusion that had to be retracted in a live investigation.

- **`LAYER_GATE_SKIPS *_above/*_below` are guard-relative** (`guard_above = incoming < selected`), so `*_above` counts frames arriving BELOW the selection, and `*_below ≈ 0` means "pinned to the top layer". They cover audio and screen as well as video. (#2552)
- **`[JITTER_BUFFER] freshness_skip {from}->{to}` renders receiver->publisher.** `set_stream_context(local_sid, peer.sid_str)` puts the LOCAL session first, despite the `from_peer` field name.
- **`videocall_video_seq_loss_per_sec` is a packet COUNT per second, not a loss fraction** — the name reads as a rate; it is `record_seq_into_reorder_window`'s booked-lost packets over the window. The forward-gap component of one booking saturates at `MAX_PLAUSIBLE_FORWARD_GAP` (4096), so a very large burst under-reads. It is not a freeze predictor: loss and a frozen tile are different measurements. (#2524)

## Build Commands

```bash
# Check with default features disabled (no optional features, e.g. netsim)
cargo check --target wasm32-unknown-unknown --no-default-features -p videocall-client

# Check default mode
cargo check --target wasm32-unknown-unknown -p videocall-client
```

## E2E Tests (Playwright)

Browser-based end-to-end tests in `e2e/` using Playwright. Tests run against the Dioxus UI (port 3001). Auth is bypassed via JWT cookie injection. See the `e2e-*` targets in the `Makefile` for available commands.

Key files:
- `docker/docker-compose.e2e.yaml` — Stack definition (Dioxus UI + shared backend)
- `e2e/playwright.config.ts` — Project configuration
- `e2e/helpers/auth.ts` — JWT session cookie injection

## Agent Usage Policy

Always delegate work to the specialized roster agents instead of making changes directly. Use the appropriate agent for each task:

**This includes technical decisions, not just code edits.** Before making any recommendation or assessment about transport protocols (WebTransport, WebSocket, QUIC, datagrams, reliability), security design, or performance trade-offs, delegate to the relevant specialist agent first. Do not reason about domain-specific behavior independently — use the agent's expertise, then relay its findings. Getting the answer wrong because you skipped the expert is worse than taking an extra minute to ask.

- **frontend-rust-webtransport-and-websocket** — All Dioxus UI changes (components, pages, styling, state management)
- **backend-rust-streaming** — All backend/API changes (Axum routes, DB queries, server logic)
- **code-reviewer** — Review all code changes before committing
- **performance-reviewer** — Performance review for low-power devices and low-bandwidth networks. Audit payload sizes, unnecessary re-renders, uncompressed assets, polling intervals, missing pagination, protobuf message sizes, bundle sizes, memory leaks, and any patterns that degrade on constrained hardware or slow connections. Should be run after substantive code changes alongside code-reviewer.
- **web-security-auditor** — Full-scope application security: backend auth/authz, API endpoints, input validation, XSS/injection, CSRF, UI trust indicators (e.g. host badges, role icons, permission displays), identity comparison logic, token handling, phishing vectors, architectural security review. Must audit both server-side AND client-side code — rendering code that conveys trust or authority is security-critical.
- **database-reviewer** — Review schema, migration, and query changes
- **integration-test-writer** — Write integration tests for new or changed features
- **deploy-sync-expert** — Update Docker/K8s configs when services or dependencies change
- **e2e-test-sync** — Create/update E2E tests when user-facing behavior changes
- **ux-ui-expert** — UI/UX design guidance, component design, visual polish, accessibility
- **bot-fleet-tooling** — All `e2e/bots-app/` changes (the synthetic-participant load tool): the TypeScript CLI/orchestrator, the Playwright bot driver, injected browser sources, the control server, the operator dashboard, the resource sampler, `docker-entrypoint.sh`, and `k8s/` fleet manifests. Not for product code, and not for Playwright specs in `e2e/tests/` (those are `e2e-test-sync`).

Run agents in parallel when tasks are independent. Always run `code-reviewer` after substantive code changes. Always run `e2e-test-sync` after any change that affects user-facing behavior — E2E tests must be updated to cover the change and must pass before the work is considered complete.

**Do not defer an E2E test by assumption.** Before claiming a user-facing change can't be tested ("needs a harness that doesn't exist"), grep for an existing spec that already stands up the needed harness — a deferral must cite that search, never an assumption. **Grep the tree that matches the change — `docs/TESTING.md` is the canonical layer inventory. Searching only `e2e/tests/` produces a false "no harness exists":** `e2e/tests/` + `e2e/helpers/` for product flows crossing UI, backend and realtime; **`dioxus-ui/tests/`** for product-UI DOM, component and routing behavior (`wasm-bindgen-test` binaries run per-PR by `pr-check-dioxus-ui-hcl.yaml` — for some behavior this is the ONLY per-PR guard, because an untagged Playwright spec never runs); `videocall-client`'s own wasm/lib suite for client internals; `e2e/bots-app/dashboard/src/__tests__/` for the operator dashboard's React DOM; and `e2e/bots-app/src/**/*.test.ts` (recursive) for the bot CLI/server. This governs **where to search** — it does not relax the Playwright requirement in the acceptance table above. If the harness exists, the test is writable: extend that spec. And "covered" means the spec has actually **run green** (local docker e2e stack or a scoped CI dispatch), not merely written — note that an untagged spec (no `@bvt0`/`@bvt1`) does **not** run in per-PR CI, so it must be validated another way before the work is considered complete.

**Never generate your own general-purpose agents.** Only use the agents listed on this roster. If no roster agent fits the task, stop everything and ask the user for direction.

## Change Impact Policy

**This is a real-time video conferencing application used by participants connecting from different parts of the world over varying network conditions.** Every code change must be evaluated with this context:

- **Consider the full lifecycle.** Before changing connection, session, or transport code, trace the complete flow: initial connection, election, reconnection, re-election, graceful disconnect, and crash recovery. Changes that fix one path must not break another.
- **Consider all transport modes.** Changes to shared connection logic must be validated against both WebTransport and WebSocket paths. A fix for one transport must not introduce regressions in the other.
- **Consider real-world networks.** Thresholds, timeouts, and retry logic must account for high-latency links (200ms+), packet loss, jitter, and mobile networks — not just localhost. Hardcoded values that only work on fast local connections are bugs.
- **Consider scale.** Meetings may have many participants. Events that fire per-connection (not per-user) can cause O(n) storms during reconnection waves. Session management, NATS publishing, and UI re-renders must all be evaluated for fan-out cost.
- **Consider the server as part of the system.** Client-side fixes that rely on server behavior (e.g., session lifecycle, event broadcasting) must verify the server actually upholds those assumptions, and vice versa. Cross-cutting changes require both frontend and backend agents.

## Change Acceptance Criteria

Every change must satisfy the applicable rules below. These are derived from recurring review findings — each rule exists because its absence caused a shipped defect or a review round-trip.

| If your change... | You MUST... |
|---|---|
| **Fixes a bug or changes runtime behavior** | Include a regression test that **FAILS on the un-fixed code**. Reverting the production change must break the test. A test that passes on both the fixed and unfixed code proves nothing. **Before declaring a side effect untestable, grep the changed file for `#[cfg(test)]` seams, `RefCell`/interior-mutable fields, and existing test accessors — the recorded data is often already there and only needs a 3-line accessor to expose it. "The decoder is a noop" does not mean the call-site arguments are unobservable.** |
| **Changes user-facing behavior** (click flow, rendered state, toast, control, overlay, route) | Include a Playwright E2E spec in `e2e/tests/` covering the new flow. "Covered" means the spec has **run green** (local docker e2e stack or scoped CI dispatch) — written-but-never-run does not count. An untagged spec (no `@bvt`) does not run in per-PR CI; validate it another way. **Applies to any change that alters the product UI's observable behavior, whatever crate it originates in** — `dioxus-ui/`, `videocall-client/`, `actix-api/`, `meeting-api/`. The only exclusion is `e2e/bots-app/`; see the row below. |
| **Changes the bots-app operator tool** (`e2e/bots-app/`) | Cover it in that tool's OWN suite, not `e2e/tests/`: React DOM in `e2e/bots-app/dashboard/src/__tests__/` (vitest + jsdom + `@testing-library/react`); CLI/control-server in `e2e/bots-app/src/**/*.test.ts` — **recursive glob on purpose**, most server tests live under `src/control/`. Both run in per-PR CI for PRs targeting `hcl-main`/`PR-staging` (`pr-check-e2e-lint-hcl.yaml`, triggered on `e2e/**`, neither step `if:`-gated), so they satisfy "demonstrated green" without a docker stack — a stronger per-PR guarantee than an untagged Playwright spec, which never runs per-PR. There is no Playwright harness for the dashboard, and building one is not the remedy. |
| **Includes a new or modified test** | The test must call or import the **production function/path** it claims to guard — not re-implement the logic inline. A test that computes the expected value the same way the production code does is testing its own copy, not the production code. |
| **Adds or modifies a comment/doc-comment making a behavioral claim** | **Prefer deleting the comment.** If it stays: the claim must be **traceable to code that delivers it**, and a *runtime* claim must have been EXECUTED, not inferred from reading. An untraceable claim means the comment is wrong **or the code is** — establish which before deleting. Rationale, measurements and history belong in the commit message, never in the comment. |
| **Touches state in encoder, connection, session, or transport code** | Trace ALL lifecycle paths for that state: cold start, reconnect (#1311 path), re-election, fatal restart, graceful disconnect, tab-background/resume. A fix for one path must not break another. `None` after cold-start and `None` after reconnect are different runtime states. |
| **Reuses a constant/threshold/interval across camera↔screen or WT↔WS** | Verify the existing values are the same across those contexts. If they DIFFER, the difference is deliberate (e.g., screen's 3s GOP for text readability vs camera's 5s). Unifying without justification is a regression. |
| **Keys off a "congestion," "pressure," "full," or "backpressure" signal** | Trace the signal to the actual queue/buffer where real backpressure surfaces. Actix mailbox `Full` is a burst absorber, NOT a receiver's downlink. Verify both transports (WS + WT). |
| **Adds recovery/exit hysteresis (consecutive-success counters, cooldown timers)** | Verify it cannot **wedge** under the condition that triggers it. Strictly-consecutive success counters reset under ongoing contention and can pin a healthy entity indefinitely. Prefer windowed/decaying/time-bounded exits. |
| **Adds a counter, metric, or log for a condition** | **Prove the condition can occur BEFORE building the instrument.** Name the code path that produces it, then walk the guard chain in its CALLERS — if a caller gate excludes the state the condition requires, the counter is dead on arrival and every test of it passes vacuously. State either one observed non-zero reading, or explicitly that it is a canary for a case never yet seen. (Real miss: a WebSocket failed-send counter whose throw requires `readyState == CONNECTING`, which every send path's `Status::Connected` gate excludes — 9 proto fields with an expected magnitude of zero.) |
| **Is a test-reliability or de-flake change** | Demonstrate the spec **actually runs green** after the fix (local docker or CI dispatch). A de-flake PR that hasn't been run proves nothing about reliability. |
| **Has a merge conflict with the base branch** | Resolve by **merging** the base branch (`git merge github01/PR-staging`) — do NOT rebase. Force-push is blocked on this repository; rebasing rewrites history and requires a force-push, which will fail and force creation of a new PR. A plain `git merge` adds a merge commit without rewriting history and can be pushed normally. |

## Source Code Rules

- **No symlinks or hardlinks for source files.** Each crate/UI must own its files independently. Do not use symlinks between source directories.

## Linter & Formatter Rules

**All code changes MUST pass project linters before being considered complete.** Agents must run the appropriate linter/formatter after editing any file:

- **Rust code:** Run `cargo fmt` on changed crates. To catch clippy warnings the way CI does, run **`make clippy-ci`** — a plain `cargo clippy` (or `cargo clippy --all`) lints only library/binary targets and MISSES `#[test]`-target lints and crate-specific feature flags. CI therefore lints each test-bearing crate's `--tests` explicitly — the authoritative list is the `clippy-ci` recipe itself, not this sentence — and these lints fail CI on an already-pushed PR if missed locally. `make clippy-ci` mirrors that exact command set from `.github/workflows/pr-check-rust-hcl.yaml`; it is the only local command that reproduces the CI clippy job. **If you add a new crate with test code, add a `--tests` clippy step to BOTH the workflow and the `clippy-ci` target** — `scripts/check-clippy-ci-sync.sh` (run by the fmt job in CI, issue #1500) fails the build if the two lists drift, so editing one without the other turns CI red — and fails separately if any workspace crate carrying test code has no `--tests` step at all (issue #2453).
- **TypeScript / JS (e2e/):** Run `cd e2e && npx prettier --write <files> && npx eslint <files> && npx tsc --noEmit` to match the CI `ci:lint` check.
- **General:** No unused imports, no unused variables, follow existing code style. Respect all project lint configs (`.eslintrc`, `rustfmt.toml`, `.prettierrc`, etc.).

This is mandatory for every agent making code changes — not optional. CI will reject PRs that fail linting.

## Adversarial Self-Review Rule (MANDATORY before "done")

**Passing linters, `cargo check`, and CI does NOT mean a change is correct.** Lint/compile prove the code is well-formed; they do not prove it does what it claims. Before declaring any change complete — and before pushing or requesting review — run an explicit adversarial pass over the diff. Apply these three checks, by hand, to every new or changed piece:

1. **Does this code path actually execute under real conditions?** Trace init order, guard conditions, lifetimes, and feature gates — not "it compiles." Ask: *under what runtime state does this line run, and is that state actually reached?* **For instrumentation, ask it of the CONDITION, not the code you added:** the increment runs fine, so the real question is whether the event it counts ever happens. That answer lives in the CALLERS' guard chain, not in the function under review. (Real miss: a `warn!` that could never fire because the level was read before the logger was installed, so the facade's `max_level()` was still `Off` and the record was dropped.)

2. **Does each new test fail if you break the thing it names?** Mentally (or actually) mutate the source the test claims to protect; if the test would still pass, it is fake and must be rewritten to reference a real source of truth. A test asserting `X == X` (a literal against itself) pins nothing. (Real miss: a "lockstep pin" test that asserted `LevelFilter::Info == LevelFilter::Info`.)

3. **Is every claim in a comment, doc, or PR description verified against the code — or merely asserted?** A comment stating a contract ("fires regardless of X", "guaranteed once") must be traced to the code that delivers it, and a *runtime* claim must have been RUN. If you can't trace it, the comment is wrong or the code is — establish which. **Then judge comment volume over the whole PR, never one commit: over ~10% of added lines blocks the PR; delete comments, rewording is not a fix.** Comments the author cannot delete do not count — licence headers, generated output, public-API doc comments. (Real miss: a doc comment claiming behavior the code path disproved.)

**This is not just for Rust — it applies equally to CI workflows, shell, YAML, Helm, Dockerfiles, and config.** These have no compiler and often no test, so they are *more* prone to the "looks right, doesn't work" defect, not less. Apply the three checks to them explicitly:
- **Check 1 on CI/shell — trace the failure and empty paths, not just the happy path.** Ask: *what does this do when the tool fails, the input is empty, the file is absent, or the command errors?* A step that reports success when its underlying command produced no result is a false green — the worst kind. Verify the trigger actually fires (e.g. a `paths:` filter must include the workflow's own file, or a PR editing it won't run it). (Real miss: a mutation-test summary that printed "all caught" when `cargo-mutants` had failed and produced no output, because it only checked for a results file that a broken run never creates.)
- **Check 3 on CI/shell — verify tool contracts against the source, don't guess.** Exit codes, flag names, and output paths must be confirmed against the tool's actual docs/source, not assumed. (Real miss: assuming a tool's exit `1` meant "nothing to test" when it actually meant "usage error" — verified only by reading the crate's `exit_code.rs`.)

**Why this rule exists:** the recurring defect in this repo's PRs has not been missing knowledge — it is verification discipline. Plausible-looking artifacts (a warning, a test, a doc claim, a CI step) get shipped without proving they do their job, and a reviewer catches them later. The `code-reviewer` agent must be run in genuinely adversarial mode — instruct it to perform checks 1–3 above, not just style/correctness at a glance. Author-mode optimism ("this looks right") is the bias to counteract. Treat a self-review that returns "PASS" while these checks were not actually performed as a review that did not happen.

This applies to every agent and to direct edits, and to every file type — code, tests, docs, CI, shell, and infra config. It is part of the definition of "complete," alongside passing linters and tests.

## Responding To A REQUEST_CHANGES Verdict

When fixing a PR after a `CHANGES_REQUESTED` review, these four steps are **mandatory** after pushing the fix commit(s):

1. **Post a PR comment** summarizing what was fixed and how each blocker was addressed. Include the commit SHA(s), a one-line description of each change, and (for test fixes) confirmation that mutation sensitivity was verified.
2. **Remove blocking labels** (`NEEDS CHANGES`, `NEEDS TESTS`, `RESOLVE CONFLICTS`) as applicable.
3. **Add `READY FOR REVIEW`**.
4. **Re-request review** from the reviewer who requested changes (`gh api --method POST repos/OWNER/REPO/pulls/PR/requested_reviewers -f "reviewers[]=LOGIN"`).

Do not leave a PR in `NEEDS CHANGES` / `NEEDS TESTS` state after pushing a fix without completing all four steps.
