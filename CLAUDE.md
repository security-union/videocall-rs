# CLAUDE.md

## Project Overview

`videocall-rs` is a Rust-based video calling platform. The main crates are:

- **videocall-client** - Client library targeting `wasm32-unknown-unknown`.
- **dioxus-ui** - Dioxus-based frontend (the sole UI, uses `videocall-client`)
- **videocall-types** - Shared protobuf types
- **videocall-codecs** - Audio/video codec wrappers

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

Run agents in parallel when tasks are independent. Always run `code-reviewer` after substantive code changes. Always run `e2e-test-sync` after any change that affects user-facing behavior — E2E tests must be updated to cover the change and must pass before the work is considered complete.

**Never generate your own general-purpose agents.** Only use the agents listed on this roster. If no roster agent fits the task, stop everything and ask the user for direction.

## Change Impact Policy

**This is a real-time video conferencing application used by participants connecting from different parts of the world over varying network conditions.** Every code change must be evaluated with this context:

- **Consider the full lifecycle.** Before changing connection, session, or transport code, trace the complete flow: initial connection, election, reconnection, re-election, graceful disconnect, and crash recovery. Changes that fix one path must not break another.
- **Consider all transport modes.** Changes to shared connection logic must be validated against both WebTransport and WebSocket paths. A fix for one transport must not introduce regressions in the other.
- **Consider real-world networks.** Thresholds, timeouts, and retry logic must account for high-latency links (200ms+), packet loss, jitter, and mobile networks — not just localhost. Hardcoded values that only work on fast local connections are bugs.
- **Consider scale.** Meetings may have many participants. Events that fire per-connection (not per-user) can cause O(n) storms during reconnection waves. Session management, NATS publishing, and UI re-renders must all be evaluated for fan-out cost.
- **Consider the server as part of the system.** Client-side fixes that rely on server behavior (e.g., session lifecycle, event broadcasting) must verify the server actually upholds those assumptions, and vice versa. Cross-cutting changes require both frontend and backend agents.

## Source Code Rules

- **No symlinks or hardlinks for source files.** Each crate/UI must own its files independently. Do not use symlinks between source directories.

## Linter & Formatter Rules

**All code changes MUST pass project linters before being considered complete.** Agents must run the appropriate linter/formatter after editing any file:

- **Rust code:** Run `cargo fmt` on changed crates. To catch clippy warnings the way CI does, run **`make clippy-ci`** — a plain `cargo clippy` (or `cargo clippy --all`) lints only library/binary targets and MISSES `#[test]`-target lints and crate-specific feature flags. CI therefore lints each test-bearing crate's `--tests` explicitly (`videocall-client` on wasm, `videocall-aq`, `videocall-codecs`, `videocall-ui`, and `neteq --no-default-features --features web`), and these lints fail CI on an already-pushed PR if missed locally. `make clippy-ci` mirrors that exact command set from `.github/workflows/pr-check-rust-hcl.yaml`; it is the only local command that reproduces the CI clippy job. **If you add a new crate with test code, add a `--tests` clippy step to BOTH the workflow and the `clippy-ci` target.**
- **TypeScript / JS (e2e/):** Run `cd e2e && npx prettier --write <files> && npx eslint <files> && npx tsc --noEmit` to match the CI `ci:lint` check.
- **General:** No unused imports, no unused variables, follow existing code style. Respect all project lint configs (`.eslintrc`, `rustfmt.toml`, `.prettierrc`, etc.).

This is mandatory for every agent making code changes — not optional. CI will reject PRs that fail linting.

## Adversarial Self-Review Rule (MANDATORY before "done")

**Passing linters, `cargo check`, and CI does NOT mean a change is correct.** Lint/compile prove the code is well-formed; they do not prove it does what it claims. Before declaring any change complete — and before pushing or requesting review — run an explicit adversarial pass over the diff. Apply these three checks, by hand, to every new or changed piece:

1. **Does this code path actually execute under real conditions?** Trace init order, guard conditions, lifetimes, and feature gates — not "it compiles." Ask: *under what runtime state does this line run, and is that state actually reached?* (Real miss: a `warn!` that could never fire because the level was read before the logger was installed, so the facade's `max_level()` was still `Off` and the record was dropped.)

2. **Does each new test fail if you break the thing it names?** Mentally (or actually) mutate the source the test claims to protect; if the test would still pass, it is fake and must be rewritten to reference a real source of truth. A test asserting `X == X` (a literal against itself) pins nothing. (Real miss: a "lockstep pin" test that asserted `LevelFilter::Info == LevelFilter::Info`.)

3. **Is every claim in a comment, doc, or PR description verified against the code — or merely asserted?** A comment that states a contract ("fires regardless of X", "guaranteed once") must be traced to the code that delivers it. If you can't trace it, the comment is wrong or the code is. (Real miss: a doc comment claiming behavior the code path disproved.)

**This is not just for Rust — it applies equally to CI workflows, shell, YAML, Helm, Dockerfiles, and config.** These have no compiler and often no test, so they are *more* prone to the "looks right, doesn't work" defect, not less. Apply the three checks to them explicitly:
- **Check 1 on CI/shell — trace the failure and empty paths, not just the happy path.** Ask: *what does this do when the tool fails, the input is empty, the file is absent, or the command errors?* A step that reports success when its underlying command produced no result is a false green — the worst kind. Verify the trigger actually fires (e.g. a `paths:` filter must include the workflow's own file, or a PR editing it won't run it). (Real miss: a mutation-test summary that printed "all caught" when `cargo-mutants` had failed and produced no output, because it only checked for a results file that a broken run never creates.)
- **Check 3 on CI/shell — verify tool contracts against the source, don't guess.** Exit codes, flag names, and output paths must be confirmed against the tool's actual docs/source, not assumed. (Real miss: assuming a tool's exit `1` meant "nothing to test" when it actually meant "usage error" — verified only by reading the crate's `exit_code.rs`.)

**Why this rule exists:** the recurring defect in this repo's PRs has not been missing knowledge — it is verification discipline. Plausible-looking artifacts (a warning, a test, a doc claim, a CI step) get shipped without proving they do their job, and a reviewer catches them later. The `code-reviewer` agent must be run in genuinely adversarial mode — instruct it to perform checks 1–3 above, not just style/correctness at a glance. Author-mode optimism ("this looks right") is the bias to counteract. Treat a self-review that returns "PASS" while these checks were not actually performed as a review that did not happen.

This applies to every agent and to direct edits, and to every file type — code, tests, docs, CI, shell, and infra config. It is part of the definition of "complete," alongside passing linters and tests.
