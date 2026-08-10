+++
title = "We Deleted Docker (and the Flakes Too)"
date = 2026-08-10
description = "14 Dockerfiles to zero, CI wall time halved, browser tests 13:19 to 2:21. The numbers from moving videocall-rs to classic Nix."
[taxonomies]
tags = ["nix", "docker", "ci", "rust", "devops", "dx"]
authors = ["Dario Lencina Talarico"]
[extra]
remote_image = "/images/we-deleted-docker-hero.svg"
+++

# We Deleted Docker (and the Flakes Too)

<div style="font-size:6rem;line-height:1;margin:0.5rem 0;">🔪</div>

Not the images. The Dockerfiles. All 14 of them.

The old setup ran Nix *inside* Docker. A nixos container per service, bind-mounting the repo, each with its own private Nix store volume. We were paying container tax to run a build system whose whole point is not needing containers. Peak cosplay.

[Last time](/posts/nixify-your-leptos-website-and-stop-compiling-your-tools/) we flaked the website and went 19→5 minutes. This time we went further and deleted the flakes too. Classic Nix: `default.nix`, `release.nix`, `shell.nix`. Inputs pinned with [nixtamal](https://nixtamal.toast.al/) — a plain KDL manifest, evaluable by any Nix since forever. Docker images are just derivations:

```bash
nix-build release.nix -A images.websocket-server | docker load
```

The Dockerfile never comes back. macOS cross-compiles aarch64-musl natively — no VM, no Linux builder. Your laptop emits ELF.

## Why kill flakes?

Because flakes are not Nix. They're a framework bolted onto Nix, and after eight years the bolt is still marked **experimental** — every command starts with a flag ritual acknowledging you're off the supported path. And what does the framework buy you? Rigidity. The `inputs` block is a dead mini-DSL: you can't type real Nix in it — no functions, no conditionals, no computing an input from another input. It's a config file cosplaying as a programming language, inside a programming language. Then the evaluator copies your whole repo into the store on every eval to enforce a purity you already had.

Classic Nix is just a language. `default.nix` is a function; you call it with `import`, compose it, override it, parameterize it — the full language, everywhere, including the pins. The one thing flakes genuinely gave us — a lockfile — nixtamal provides in ~40 lines of KDL without taking the language away. Stability, power, and the lock. Pick three.

## The scoreboard

| | before | after |
|---|---|---|
| Dockerfiles | 14 | **0** |
| flakes | 1 | **0** |
| containers in the dev loop | all of them | **0** |
| CI image gate, wall clock | ~10:00 (worst leg 22:00) | **5:05** |
| dioxus-ui browser test job | 13:19 | **2:21** |

79 files, +3540/−1844. Dev is process-compose now: postgres 18, NATS, prometheus, grafana and every server under cargo-watch — native processes, health-gated startup, zero Docker on the hot path. Integration tests spin a throwaway native stack. Env layering is `defaults < .env < .env.local`, last one gitignored.

## Numbers, because everything else is marketing

![CI image build wall time: docker typical 10:00, docker worst leg 22:00, nix + crane + cachix 5:05](/images/we-deleted-docker-ci-wall.svg)

The trick isn't Nix, it's caching done honestly. [crane](https://crane.dev) splits dependency compilation into its own derivation whose hash ignores source churn — verified at the unit level: the same web-sys unit hash across commits, or you don't have a cache, you have a vibe. Cachix serves it back to all 13 CI jobs. Dependencies of dependencies compile once, ever.

![dioxus-ui browser test job: serial typical 13:19, serial with 4 wedges about 22:50, parallel with 4 wedges 13:22, parallel clean 2:21](/images/we-deleted-docker-ui-tests.svg)

Headless Chrome sometimes wedges its renderer at session startup for exactly 300 seconds. The browser tests ran serially, so one wedged renderer blocked the line for 5 minutes, about twice per run. We don't fight the timeout — we overlap it: prebuild every test binary once, then drive `wasm-bindgen-test-runner` directly, 3 sessions at a time. You can't get this through `cargo test` — parallel invocations serialize on the target-dir lock and your parallelism is fake. Clean run: **2:21**. The worst measured run had four wedges and still matched the old baseline's *best* day.

## Lessons

1. If your build needs a Dockerfile, your build has state you haven't admitted to. Derivations force the confession.
2. Measure the cache at the derivation-hash level or you don't have a cache, you have a vibe.
3. The 300-second timeout was never the bug. The serialism was.

17/17 e2e green. All numbers from CI logs on [PR #897](https://github.com/security-union/videocall-rs/pull/897).
