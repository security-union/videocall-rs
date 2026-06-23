# videocall-rs Codex Instructions

This is a Rust-based real-time video conferencing platform. Participants connect
via WebTransport or WebSocket from phones, Chromebooks, satellite links, and
desktop browsers worldwide. The relay (actix-api/) forwards media per-receiver;
the client (videocall-client/, dioxus-ui/) decodes and renders in the browser
via wasm32. Latency, scale (20+ participants), and transport diversity are
first-class constraints.

## Project Overview

- `videocall-client` - client library targeting `wasm32-unknown-unknown`
- `dioxus-ui` - Dioxus-based frontend and the sole UI; uses `videocall-client`
- `videocall-types` - shared protobuf types
- `videocall-codecs` - audio/video codec wrappers

## Build Commands

```bash
cargo check --target wasm32-unknown-unknown --no-default-features -p videocall-client
cargo check --target wasm32-unknown-unknown -p videocall-client
```

## E2E Tests

Browser-based E2E tests live in `e2e/` and use Playwright against the Dioxus UI on port 3001. Auth is bypassed through JWT cookie injection.

Key files:

- `docker/docker-compose.e2e.yaml`
- `e2e/playwright.config.ts`
- `e2e/helpers/auth.ts`

See the `e2e-*` targets in the `Makefile` for available commands.

Choose the lowest test layer that proves the behavior:

- Use unit or backend integration tests when they can catch the bug.
- Use Dioxus browser integration tests for DOM, browser APIs, component behavior, and lightweight routing.
- Use Playwright E2E when behavior crosses UI, backend, and realtime boundaries, involves multiple participants, or depends on WebSocket/WebTransport behavior.

## Change Impact Policy

Every code change must be evaluated in the context of a real-time conferencing app running across diverse networks and devices.

- Consider the full lifecycle before changing connection, session, encoder, or transport code: initial connection, election, reconnection, re-election, graceful disconnect, tab background/resume, and crash or fatal restart recovery.
- Shared connection logic must be validated against both WebTransport and WebSocket paths.
- Thresholds, timeouts, and retry logic must account for high-latency links, packet loss, jitter, and mobile networks, not just localhost.
- Check fan-out and scaling costs. Events that fire per connection can cause O(n) storms during reconnect waves.
- Client-side fixes that rely on server behavior must verify that the server actually upholds those assumptions, and server-side fixes must verify the client-side behavior they depend on.

## Source Code Rules

- No symlinks or hardlinks for source files. Each crate or UI must own its files independently.
- WebSocket and WebTransport transport adapters have protocol-specific differences by design. Do not mechanically consolidate adapter I/O, keep-alive, or send-path code just because the high-level behavior is shared.
- Adaptive-quality thresholds, timing, tier, and tuning values should stay centralized in `videocall-aq/src/constants.rs` (re-exported as `videocall_client::adaptive_quality_constants::*` via the shim in `videocall-client/src/lib.rs`). Do not scatter magic numbers across encoders, PID/controller logic, or connection code.
- `WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT` in `actix-api/src/constants.rs` is the source of truth for WebTransport outbound queue depth. The Helm env override is redundant; raise the value only for exceptional workloads because deep queues buffer stale video for slow receivers.

## Runtime Config Files

- `dioxus-ui/scripts/config.js` is a committed fallback and is also rewritten by the E2E/dev container from environment variables. Do not casually stage it while the E2E stack is running; check whether changes are intentional source edits or generated env noise.
- When adding a field to the wasm `RuntimeConfig`, either give `dioxus-ui/scripts/config.js` a value that works against a vanilla `make e2e-up` stack or make the field optional with `#[serde(default)]`.

## Linter And Formatter Rules

All code changes must pass the project linters before the work is considered complete.

- Rust: run `cargo fmt` on changed crates. To match CI clippy behavior, run `make clippy-ci`; plain `cargo clippy` or `cargo clippy --all` misses test targets and crate-specific feature flags that CI checks.
- If adding a new crate with test code, add a `--tests` clippy step to both `.github/workflows/pr-check-rust-hcl.yaml` and the `clippy-ci` Makefile target. `scripts/check-clippy-ci-sync.sh` fails CI if the lists drift.
- TypeScript/JS in `e2e/`: run `cd e2e && npx prettier --write <files> && npx eslint <files> && npx tsc --noEmit`.
- Do not leave unused imports or variables.
- Respect local lint and formatter configuration.

## Change Acceptance Criteria

Every change must satisfy the applicable rules below. These are derived from recurring review findings; each rule exists because its absence caused a shipped defect or a review round-trip. Keep this table aligned with the corresponding table in `CLAUDE.md`.

| If your change... | You MUST... |
|---|---|
| **Fixes a bug or changes runtime behavior** | Include a regression test that **FAILS on the un-fixed code**. Reverting the production change must break the test. A test that passes on both the fixed and unfixed code proves nothing. |
| **Changes user-facing behavior** (click flow, rendered state, toast, control, overlay, route) | Include a Playwright E2E spec in `e2e/tests/` covering the new flow. "Covered" means the spec has **run green** (local docker e2e stack or scoped CI dispatch); written-but-never-run does not count. An untagged spec (no `@bvt`) does not run in per-PR CI; validate it another way. |
| **Includes a new or modified test** | The test must call or import the **production function/path** it claims to guard, not re-implement the logic inline. A test that computes the expected value the same way the production code does is testing its own copy, not the production code. |
| **Adds or modifies a comment/doc-comment making a behavioral claim** | The claim must be **traceable to code that delivers it**. "Fires regardless of X" requires a code path that provably fires regardless of X. If the claim doesn't match the code, either fix the code or fix the comment; never ship the contradiction. |
| **Touches state in encoder, connection, session, or transport code** | Trace ALL lifecycle paths for that state: cold start, reconnect (#1311 path), re-election, fatal restart, graceful disconnect, tab-background/resume. A fix for one path must not break another. `None` after cold-start and `None` after reconnect are different runtime states. |
| **Reuses a constant/threshold/interval across camera↔screen or WT↔WS** | Verify the existing values are the same across those contexts. If they DIFFER, the difference is deliberate (e.g., screen's 3s GOP for text readability vs camera's 5s). Unifying without justification is a regression. |
| **Keys off a "congestion," "pressure," "full," or "backpressure" signal** | Trace the signal to the actual queue/buffer where real backpressure surfaces. Actix mailbox `Full` is a burst absorber, NOT a receiver's downlink. Verify both transports (WS + WT). |
| **Adds recovery/exit hysteresis (consecutive-success counters, cooldown timers)** | Verify it cannot **wedge** under the condition that triggers it. Strictly-consecutive success counters reset under ongoing contention and can pin a healthy entity indefinitely. Prefer windowed/decaying/time-bounded exits. |
| **Is a test-reliability or de-flake change** | Demonstrate the spec **actually runs green** after the fix (local docker or CI dispatch). A de-flake PR that hasn't been run proves nothing about reliability. |
| **Has a merge conflict with the base branch** | Rebase clean before requesting review. Red CI from a merge conflict is a blocker regardless of code quality. |

## Mandatory Adversarial Review

Use the repository skill `$videocall-adversarial-review` for both of these workflows:

- Before creating, updating, or marking a PR ready, run its **pre-submit mode** against the complete base-to-head diff. Fix blockers and rerun affected validation before proceeding.
- Whenever reviewing or re-reviewing a PR, run its **pull-request mode**, including complete conversation collection, independent verification of every prior sub-finding, current-head CI and mergeability checks, a formal verdict, and terminal label reconciliation.

This review is mandatory unless the user explicitly requests a WIP push or asks to skip it. A WIP exception does not permit describing the change as review-ready.

Non-negotiable review rules:

1. Read every formal review, issue comment, inline comment, and commit relevant to the current PR head. Do not truncate long bodies.
2. Verify every sub-point of a multi-part finding independently against current code. Classify prior findings as resolved/stale, live, or over-indexed with evidence.
3. Classify the change and enforce the skill's test obligations. Missing required tests, tests that bypass production code, and tests that would pass on unfixed code block approval.
4. Trace real execution paths, lifecycle states, both transports, network conditions, scale behavior, cross-layer assumptions, and failure cleanup wherever applicable.
5. Compile or run test targets; plain `cargo check` is not enough when Rust test code is part of the verdict.
6. Verify required CI on the exact reviewed head SHA and investigate failing logs. Do not infer that a failure is a flake.
7. Check mergeability immediately before approval. Conflicts, required-test gaps, unexplained red CI, and code blockers are incompatible with approval.
8. Lead with concrete findings ordered by severity and supported by file:line evidence. Do not manufacture findings or block on taste.

## Verification Checklist

1. **Mutation sensitivity**: Tests must fail when the production code they guard is reverted. A test that re-implements production logic inline (instead of calling the production function) is NOT testing the production code — flag it.

2. **Lifecycle paths**: All state changes in encoder/connection/session/transport code must be traced through: cold start, reconnect, re-election, fatal restart, graceful disconnect, tab-background/resume. A value that means one thing on cold start may mean something different mid-session after a partial reset.

3. **Design intent preservation**: When constants/intervals are reused across camera+screen or WebTransport+WebSocket, check whether the existing values DIFFER between those contexts. If they do, the difference is deliberate — unifying them without justification is a regression.

4. **Both transports**: Changes to shared logic must work for both WebTransport and WebSocket. A fix for one must not regress the other.

5. **Signal semantics (relay code)**: For any trigger keyed on "congestion"/"drop"/"full"/"backpressure", verify the signal reads the ACTUAL queue/buffer where the condition surfaces — not a proxy that correlates in some cases (e.g., actix mailbox Full is a burst absorber, NOT per-receiver downlink backpressure).

6. **Execution path**: Changed code must actually execute under real runtime conditions. Trace init order, guard conditions, lifetimes, feature gates, failure paths, empty inputs, missing files, and command errors.

7. **Claim accuracy**: Every claim in a comment, doc, log message, test name, or PR description must be verified against the code.

8. **E2E coverage**: Before declaring an E2E or integration test deferred because a harness does not exist, grep `e2e/tests/` and relevant unit-test modules for an existing harness. A user-facing change is not done until its E2E spec exists and has been demonstrated green through the local docker E2E stack or a scoped CI dispatch. An untagged spec without `@bvt0` or `@bvt1` does not run in per-PR CI and must be validated another way.

Apply this checklist to Rust, TypeScript, CI workflows, shell, YAML, Helm, Dockerfiles, and config.

## Pre-Submission Review

Before pushing or creating a PR, run the **pre-submit mode** of `$videocall-adversarial-review` unless the user explicitly says to skip it. At minimum:

- Run `make clippy-ci`.
- Run `cargo fmt --all --check`.
- For substantive changes, use an independent review agent when available, or perform the skill's equivalent fresh-context adversarial review.
- Route domain-specific changes to the right kind of review: backend/relay/transport, frontend/client transport, security, database/schema/wire format, E2E test sync, and UX/accessibility.
- Do not push if the gate finds blocking issues. Fix findings first, then rerun the gate.

Skip only for WIP commits, pure merge/rebase operations with no new code, or when the user explicitly says to skip.

Escalate further for changes spanning 5+ files of core transport/session/auth logic, security-adjacent changes, or schema/wire-format changes.

## Code Review Output Format

When the user asks for a code review, report problems first, ordered by severity, with file:line references. Do not add praise or a generic summary. If zero problems are found, say `No issues found.` For pull-request reviews, also include the prior-finding audit, evidence gaps, formal verdict, and label outcome required by `$videocall-adversarial-review`.
