# Adversarial Review Rubric

## Review Standard

Review the change that will actually merge, not the author's intent or the last commit. Findings must identify a concrete failure mode, affected runtime condition, and file:line evidence. Prioritize correctness, security, data loss, privacy, user-visible regressions, test validity, performance at scale, and operational failure.

Use these severities:

- **P0**: exploitable security/privacy failure, data loss, outage, or broadly unusable product.
- **P1**: likely correctness or user-visible regression, panic in production, missing required protection, or test/CI gap that can admit a defect.
- **P2**: concrete lower-risk maintainability, observability, or performance issue worth fixing or tracking.

Do not manufacture findings. Style preferences without demonstrated impact are not blockers.

## Change Classification And Test Obligations

A change can occupy multiple rows; satisfy every applicable obligation.

| Change class | Required evidence |
|---|---|
| Backend logic, API handlers, auth/authz, or DB queries | Integration coverage of the happy path and at least one realistic failure or denied path. Unit-test non-trivial pure helpers. |
| Wire format, protobuf, or serialization | Encode/decode round-trip test. Add transport integration coverage when behavior crosses the transport boundary. |
| Frontend component logic, signals, parsing, validation, or state transitions | Unit tests against production functions; use `wasm-bindgen-test` for WASM-only logic. |
| User-facing UI behavior | Playwright coverage of the actual click/input/rendered flow, demonstrated green. Visual-only CSS with no behavior change is exempt. |
| Bug fix or runtime behavior change | Regression test that fails on the unfixed production code. Reverting the fix must break the test. |
| Test reliability or de-flake change | Demonstrated green execution after the fix; a written but unexecuted test is insufficient. |
| Refactor with no behavior change | Verify existing tests execute the touched paths. New tests are unnecessary only when that evidence exists. |
| Documentation-only, pure deletion, infrastructure-only, or dependency bump without API change | No new automated test by default; verify claims, syntax, smoke evidence, and applicable CI. |
| Cherry-pick already tested upstream | Link and verify the upstream tests. Otherwise apply the underlying change class. |

Missing required tests always blocks approval. There is no approve-now/add-tests-later path.

## Test Adequacy

For every new or modified test:

1. Read the body and assertions, not only the name.
2. Confirm it calls/imports the production path rather than duplicating its algorithm.
3. Confirm it exercises the changed branch and realistic failure mode.
4. Establish mutation sensitivity by reverting or otherwise disabling the production fix when practical. If not practical, explain precisely why the pre-fix behavior fails the assertion. **Before declaring a side effect untestable, grep the file for `#[cfg(test)]` seams, `RefCell`, interior-mutable fields, and existing test accessors — the data is often already recorded and just needs a 3-line accessor. "The decoder is a noop" does not mean the call-site arguments are unobservable.**
5. Check fixtures and expected values are independent of the constant or implementation being guarded.
6. Compile test targets. For Rust, plain `cargo check` does not compile all `#[cfg(test)]` and integration-test code; use `cargo check --tests`, `cargo test --no-run`, or run the tests.
7. For E2E, ensure the test traverses the actual user flow and ran green in the local stack or a scoped CI dispatch.

## Runtime And Architecture Analysis

Apply all relevant checks:

- **Execution path**: Trace initialization, guards, feature flags, inputs, async lifetimes, error handling, and cleanup. Confirm the changed code runs under the claimed condition.
- **Lifecycle**: For encoder/connection/session/transport state, cover cold start, reconnect, re-election, fatal restart, graceful disconnect, crash recovery, and tab background/resume.
- **Transports**: Validate shared behavior against WebSocket and WebTransport. Preserve deliberate protocol differences.
- **Media contexts**: Do not unify camera and screen constants without verifying existing values and design intent.
- **Network reality**: Evaluate 200ms+ latency, loss, jitter, mobile transitions, and stale buffered media.
- **Scale**: Check per-connection fan-out, reconnection waves, NATS publishing, actor mailboxes, allocations, and UI rerenders for O(n) storms or worse.
- **Signal semantics**: Trace congestion/backpressure/full signals to the actual queue or buffer. A proxy signal is not proof of receiver downlink pressure.
- **Recovery**: Ensure consecutive-success counters, cooldowns, and hysteresis cannot wedge under the condition they are meant to escape. Prefer bounded or decaying exits.
- **Cross-layer contract**: Verify client assumptions against server behavior and vice versa.
- **Security**: Review trust boundaries, authorization, information leakage, panic paths, untrusted URLs, and input parsing. For OAuth/auth, check open redirects, single-use CSRF state consumption, `unwrap()` in HTTP handlers, and error leakage.
- **Claims**: Verify comments, logs, test names, docs, and PR descriptions against executable code.
- **Root pattern**: Search for every occurrence of the same bug or risky pattern. Distinguish intentional variants from missed fixes.

## Existing Conversation Audit

Before forming a PR verdict, read all formal reviews, issue comments, inline comments, and commits. Do not truncate long comment bodies.

- Verify automated and human findings against the current head; neither is automatically correct.
- Split multi-point findings into independently verified sub-points. Never mark a group resolved after checking only part of it.
- Treat later correction/retraction comments as superseding earlier findings.
- Evaluate author explanations and linked fixes rather than restarting an answered conversation.
- Mark each prior item **resolved/stale**, **live**, or **over-indexed**, with file:line or commit evidence.
- Respect an existing approval as context, but raise a new blocker when independently demonstrated.
- On re-review, audit the previous review for stale-base assumptions, abstract pattern matching detached from product context, and checks the project does not enforce.

## CI, Conflicts, And Verdict

- Check CI for the exact current head SHA. Pending required checks defer approval.
- Read failing logs. Classify failures as PR-introduced, pre-existing but merge-blocking, or proven transient flakes.
- A CI-plumbing PR must demonstrate the newly enabled check actually runs and passes.
- Check GitHub mergeability immediately before the verdict. Conflicts always block approval because conflict resolution changes the reviewed result.
- Approve only when there are no blockers, required tests are adequate, required CI is green, and the PR is mergeable.
- Request changes for code blockers or missing required tests. For self-authored PRs, use a comment when GitHub prevents self-review and apply the equivalent blocking labels.

## Output

Lead with findings ordered P0 to P2. Each finding should state severity, location, runtime consequence, and required correction. Then include unresolved evidence gaps and the prior-finding audit when applicable. Do not add praise or a generic summary. If there are no findings, say `No issues found.` before the evidence/verdict block required by the PR lifecycle.
