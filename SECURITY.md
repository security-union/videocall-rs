# Security Policy

If you found a security problem in videocall.rs, thank you. Please tell us privately first so we can fix it before it's public.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting:

**https://github.com/security-union/videocall-rs/security/advisories/new**

Please don't open a public issue, pull request, or discussion for a security bug.

A good report includes:

- The affected component and version (crate version, commit SHA, or "the hosted app at app.videocall.rs").
- What an attacker can do, and what they need first (an account, a meeting link, host rights, network position).
- Steps to reproduce, or a proof of concept.
- Any logs, packet captures, or screenshots that help.
- Whether you've told anyone else, and how you'd like to be credited.

## What to expect

We're a small team, so these are targets, not promises:

- **Acknowledgement** within 5 business days.
- **Initial assessment** (confirmed or not, rough severity) within 14 days.
- **Updates** at least every couple of weeks while we work on a fix.

We'll keep the discussion in the private advisory, and we may invite you to a temporary private fork to check the fix.

## Coordinated disclosure

We ask that you give us up to 90 days to ship a fix before disclosing publicly. If a fix lands sooner, we'll publish the advisory then, and we're happy to agree on a date with you. If something is being actively exploited, we'll move faster.

When we publish a GitHub Security Advisory, we credit the reporter by name or handle unless you'd rather stay anonymous. We request a CVE through GitHub when it's warranted.

## Supported versions

We don't maintain old release branches. Fixes go to:

- **`main`**, which the hosted service at app.videocall.rs is built from. Every push to `main` builds new container images, and we roll them out to production with a manual deploy.
- **The latest published release** of each crate on crates.io (for example `videocall-client`, `videocall-types`, `videocall-cli`, `videocall-meeting-client`). Crates are released from `main` with release-plz, so a fix ships as a new patch or minor release.

If you're on an older version, the fix is to upgrade.

## Security model

Media is encrypted in transit only. WebSocket connections use TLS 1.3, and WebTransport uses QUIC's built-in TLS 1.3, so traffic is protected between each client and the relay server. videocall.rs does **not** have end-to-end encryption: the relay can see the media it forwards. The client contains older encryption code, but it's inactive and not supported, so please don't rely on it. Access control is JWT-based, with SSO/OAuth sign-in and meeting host controls.

## Scope

In scope:

- Relay servers in `actix-api` (WebSocket and WebTransport).
- `meeting-api` (meeting management, auth, JWT handling).
- `videocall-client` and the other crates in this repo.
- `dioxus-ui`, including anything that shows identity or trust, like host badges.
- The mobile SDKs (`videocall-sdk`) and `videocall-cli`.
- The Helm charts in `helm/`, when the shipped defaults are insecure.
- The hosted service: `app.videocall.rs`, `api.videocall.rs`, and the relay servers at `*.webtransport.video`.

Out of scope:

- Bugs in third-party dependencies. Please report those upstream. If our use of a dependency makes it exploitable, that's in scope.
- Denial of service through volumetric traffic or plain resource exhaustion.
- Problems caused by misconfiguring a self-hosted deployment (weak secrets, exposed admin ports, missing TLS).
- Social engineering, phishing of maintainers or users, and physical attacks.
- Automated scanner output with no demonstrated impact.

Please don't test against other people's meetings or data on the hosted service. Use your own accounts, or run the stack locally.
