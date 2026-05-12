# Documentation Index

This directory contains long-form documentation for **videocall-rs**. The top-level
[`README.md`](../README.md) links here from its **Documentation** section as the
single entry-point for the docs graph.

If you are looking for a per-crate README (e.g. `videocall-client`, `dioxus-ui`,
`actix-api`), see the **Project Structure** section of the top-level
[`README.md`](../README.md#project-structure).

## Meeting features

- [Meeting Ownership and Architecture](MEETING_OWNERSHIP.md) — ownership model,
  two-service token flow, and meeting lifecycle.
- [Meeting API Reference](MEETING_API.md) — full HTTP endpoint catalogue for the
  meeting service.
- [Searching for Meetings](SEARCH.md) — end-user behaviour of the search modal.
- [Postman Collection](postman/README.md) — pre-built Postman environment for
  the meeting API.

## Architecture and design

- [Architecture Document](../ARCHITECTURE.md) — high-level system architecture
  and the WebTransport rationale.
- [Global High-Availability Deployment Design](../GLOBAL_DEPLOYMENT.md) —
  multi-region active/active design for the production deployment.
- [Design: Decouple videocall-client from Yew](DESIGN-decouple-yew.md) —
  framework-agnostic refactor of the shared client crate.
- [Design: WebTransport Actor Consolidation](../actix-api/docs/DESIGN-webtransport-actor-consolidation.md) —
  Actix actor refactor that mirrors the WebSocket implementation.
- [Performance Optimization Plan](PERFORMANCE_OPTIMIZATION_PLAN.md) — performance
  roadmap and tracked workstreams.
- [Metrics: Problems, Work Done, and Path Forward](METRICS_PROBLEMS_AND_ROADMAP.md) —
  audit of the observability stack and the roadmap to close the gaps.
- [Plan: Identity Cleanup](PLAN-identity-cleanup.md) — multi-stage plan to
  separate session id, user id, and display name across the stack.

## Operations and deployment

- [VC2 Deployment Runbook](VC2_DEPLOYMENT_RUNBOOK.md) — building, pushing, and
  rolling out images to the VC2 cluster.
- [Production Monitoring Stack](Monitoring_Production.md) — Prometheus, Grafana,
  Loki, and alerting topology.
- [Server Sizing Guide](server-sizing-guide.md) — sizing a single-node
  (non-HA) Kubernetes deployment.
- [OAuth Helm Configuration](OAUTH_HELM_CONFIGURATION.md) — provider-specific
  Helm values for Google, Okta, and other OIDC providers.
- [PR Previews](PR_PREVIEWS.md) — per-PR sandbox stack used to test WebRTC and
  OAuth flows against real infrastructure.
- [Helm Charts](../helm/README.md) — Helm-based Kubernetes deployment guide.

## Testing

- [Testing Overview](TESTING.md) — the three automated test layers (Playwright,
  Dioxus browser tests, backend integration tests) and how they fit together.
- [Backend Testing Guide (actix-api)](../actix-api/TESTING.md) — patterns,
  PostgreSQL/NATS fixtures, and writing new tests for the backend.

## Performance investigations and post-mortems

- [Scrum Call 2026-04-13 Performance Investigation](2026-04-13-scrum-call-performance-investigation.md) —
  detailed write-up of the meeting performance regression.
- [Server-Side Metrics Gap Analysis (2026-04-13)](2026-04-13-server-metrics-gap-analysis.md) —
  companion gap analysis filed alongside the scrum-call investigation.
- [Performance Notes](../PERFORMANCE.md) — top-level performance notes
  (work in progress).

## Change logs and migration notes

- [Identity Cleanup CHANGELOG](CHANGELOG-identity-cleanup.md) — record of the
  identity-cleanup refactor (`email` → `user_id`, `username` → `display_name`).
- [Search Query Optimization CHANGELOG](CHANGELOG-search-query-optimization.md) —
  search modal query path optimisation notes.

## Planning and RFCs

- [RFC-1: q3-q4-2023 Planning](../rfc/rfc-1-q3-q4-2023-planning.md) — earliest
  formal RFC, kept for historical reference.

## Setup guides

- [macOS Apple Silicon Setup Guide](../MACOS_APPLE_SILICON_INSTALLATION_GUIDE.md) —
  M1/M2/M3/M4 installation notes.
- [Contributing Guidelines](../CONTRIBUTING.md) — how to contribute, including
  the [Code of Conduct](../CODE_OF_CONDUCT.md).

## Tooling and scripts

- [parse_meeting_console_logs.sh](../scripts/parse_meeting_console_logs.README.md) —
  fast structured summary of a meeting's browser console logs.
- [Opensource workflows (archived)](../.github/workflows-opensource/README.md) —
  legacy GitHub Actions workflows that targeted the public OSS mirror.
- [CLAUDE.md](../CLAUDE.md) — Claude Code agent and code-quality policy for
  this repository.

## Frontend styling

- [dioxus-ui styling tokens](../dioxus-ui/docs/styling-tokens.md) — practical
  guide to the current dioxus-ui token system.

## Synthetic-bot load testing

- [bot README](../bot/README.md) — overview of the synthetic-client bot used
  for load testing.
- [Bot adaptive-quality / network-impairment validation log](../bot/VALIDATION.md) —
  rolling log of what the bot's network-condition simulation has been proven
  to do.

## Native mobile SDKs

- [videocall-sdk overview](../videocall-sdk/README.md) — iOS (Swift) and
  Android (Kotlin) bindings for the WebTransport client.
- [VideoCallKit (Swift Package)](../videocall-sdk/VideoCallKit/README.md) —
  the Swift Package that wraps the Rust bindings for iOS apps.

## NetEq audio jitter buffer

- [neteq overview](../neteq/README.md) — the adaptive jitter buffer used by
  the audio pipeline.
- [NetEq Performance Dashboard](../neteq/DASHBOARD_README.md) — real-time web
  dashboard for monitoring NetEq buffer and network statistics.

## Engineering vlog

- [Engineering Vlog Index](../engineering-vlog/README.md) — index of long-form
  engineering posts published from this repository.
