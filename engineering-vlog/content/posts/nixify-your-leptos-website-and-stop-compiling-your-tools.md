+++
title = "Nixify Your Leptos Website and Stop Compiling Your Tools"
date = 2026-02-18
description = "How we cut CI build times from 19 to 5 minutes by replacing apt-get and cargo install with a Nix flake — and we haven't even started caching compiled Rust crates yet."
[taxonomies]
tags = ["nix", "leptos", "rust", "docker", "ci", "devops", "dx"]
authors = ["Dario Lencina Talarico"]
+++

# Nixify Your Leptos Website and Stop Compiling Your Tools

Every time I ran CI on our [Leptos website](https://github.com/security-union/videocall-rs), I watched 19 minutes of my life drain away. A big chunk of that time was spent compiling *the tools that compile our code*. `cargo install cargo-leptos`, `cargo install wasm-bindgen-cli`, downloading Node from a sketchy shell-pipe `curl | bash`, running `apt-get update` to install `libssl-dev` and friends. Every. Single. Build.

This is insane. These tools don't change between builds. They're the same binaries every time. Yet we were compiling them from source on every CI run and every Docker build like it was 2016 and we all had Ubuntu Xenial laptops.

So I ripped it all out and replaced it with a [Nix flake](https://github.com/security-union/videocall-rs/pull/631). Build time went from 19 minutes to 5 minutes. And I haven't even started using Nix to cache compiled Rust crate dependencies yet.

## The Before: A Horror Story in YAML and Dockerfile

The Dockerfile was grim:

```dockerfile
FROM rust:1.83-slim-bookworm as builder

RUN rustup default nightly-2024-11-01

RUN apt-get update && apt-get install -y \
    libssl-dev pkg-config g++ git-all curl \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs

RUN cargo install --locked cargo-leptos@0.2.29
RUN cargo install wasm-bindgen-cli@0.2.100 --locked
```

Piping a shell script from `deb.nodesource.com` into `bash` as root inside a Docker build. Compiling `cargo-leptos` and `wasm-bindgen-cli` from source every time the layer cache misses. This is how the industry works and it's embarrassing.

## The After: A Clean `flake.nix`

Here's the entire `flake.nix`:

```nix
{
  description = "videocall-rs - WebTransport video calling platform";

  inputs = {
    nixpkgs.url =
      "github:NixOS/nixpkgs/ee09932cedcef15aaf476f9343d1dea2cb77e261";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustNightly = pkgs.rust-bin.nightly."2024-11-01".default.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [ "rust-src" "rust-analyzer" ];
        };
      in {
        devShells.leptos-website = pkgs.mkShell {
          nativeBuildInputs = [
            rustNightly
            pkgs.cargo-leptos
            pkgs.wasm-bindgen-cli_0_2_100
            pkgs.nodejs_20
            pkgs.binaryen
            pkgs.pkg-config
            pkgs.openssl
            pkgs.git
          ];
          LEPTOS_HASH_FILES = "false";
          LEPTOS_TAILWIND_VERSION = "v3.4.17";
        };
        devShells.default = self.devShells.${system}.leptos-website;
      });
}
```

Every tool version is pinned. `cargo-leptos`, `wasm-bindgen-cli`, Node 20, the exact Rust nightly — all declared in one place, all pulled as pre-built binaries from the Nix binary cache. No compilation. No `apt-get`. No `curl | bash`.

## The New CI: 38 Lines

The entire CI workflow collapsed from 99 lines to 38:

```yaml
steps:
  - uses: actions/checkout@v4

  - uses: DeterminateSystems/nix-installer-action@main
  - uses: DeterminateSystems/magic-nix-cache-action@main

  - name: Cache cargo dependencies
    uses: actions/cache@v4
    with:
      path: |
        ~/.cargo/registry
        ~/.cargo/git
        leptos-website/target
      key: ${{ runner.os }}-cargo-leptos-${{ hashFiles('leptos-website/Cargo.lock') }}

  - name: Build Leptos website
    run: |
      nix develop .#leptos-website --command bash -c "\
        cd leptos-website && \
        npm install && \
        cargo leptos build --release"
```

Two Nix actions install Nix and set up the cache. One cargo cache for actual project dependencies. One build step. That's it.

The `magic-nix-cache-action` is the secret weapon here — it transparently caches all the Nix store paths that `nix develop` pulls, so subsequent CI runs get the entire toolchain in seconds instead of downloading from `cache.nixos.org`.

## The New Dockerfile: No apt-get, No cargo install

```dockerfile
FROM nixos/nix:2.33.2 AS builder

ENV NIX_CONFIG="experimental-features = nix-command flakes"

WORKDIR /app

COPY flake.nix flake.lock ./
RUN git init && git add flake.nix flake.lock
RUN nix develop .#leptos-website --command true

COPY leptos-website/ leptos-website/
RUN git add leptos-website/

RUN nix develop .#leptos-website --command bash -c "\
    cd leptos-website && \
    npm install && \
    cargo leptos build --release"

FROM debian:bookworm-slim

COPY --from=builder /app/leptos-website/target/release/leptos_website /app/
COPY --from=builder /app/leptos-website/target/site /app/site
COPY --from=builder /app/leptos-website/Cargo.toml /app/

WORKDIR /app
ENV RUST_LOG="info"
ENV LEPTOS_SITE_ADDR="0.0.0.0:8080"
ENV LEPTOS_SITE_ROOT="site"
EXPOSE 8080
CMD ["/app/leptos_website"]
```

The `COPY flake.nix flake.lock` + `RUN nix develop --command true` pattern is the key move. It downloads all tools from the Nix binary cache and Docker caches this layer. As long as `flake.nix` and `flake.lock` don't change, this layer is instant. Your actual code changes only trigger the final build step.

The `git init && git add` is a quirk — Nix flakes require files to be tracked by Git to be visible. Small price to pay.

## Why This Works: Nix Binary Cache vs. cargo install

`cargo install cargo-leptos` downloads the source code for `cargo-leptos` and all its dependencies, then compiles the whole thing from scratch. 10+ minutes on CI hardware. Every time.

Nix doesn't do this. When you declare `pkgs.cargo-leptos` in your flake, Nix checks `cache.nixos.org` for a pre-built binary that matches the exact nixpkgs revision you pinned. If it exists (it almost always does for packages in nixpkgs), it downloads the binary. Done. Seconds, not minutes.

`apt-get` works the same way — pre-built binaries. But `apt-get` can't give you `cargo-leptos` or `wasm-bindgen-cli` at specific versions. Nix can, because nixpkgs is a massive repository of build recipes that doubles as a binary cache. Same reproducibility as building from source, but you're downloading a binary.

## What I Haven't Done Yet

The 5-minute build time is still mostly spent compiling *our own Rust code*. I haven't set up Nix to cache compiled crate dependencies yet. Tools like [crane](https://github.com/ipetkov/crane) or [naersk](https://github.com/nix-community/naersk) can build a Nix derivation of just your Cargo dependencies, cache that in the Nix store, and only recompile your actual source files on changes.

That's the next step. I expect it'll shave off another 2-3 minutes.

## The Takeaway

If your Rust CI pipeline spends more time installing tools than compiling your code, you're doing it wrong. A single `flake.nix` replaces:

- `actions-rs/toolchain` — Nix provides the exact Rust nightly with WASM targets
- `cargo install cargo-leptos` — pre-built binary from nixpkgs
- `cargo install wasm-bindgen-cli` — pre-built binary from nixpkgs
- `setup-node` — `pkgs.nodejs_20`
- `apt-get install libssl-dev pkg-config g++` — `pkgs.openssl` and `pkgs.pkg-config`
- `curl | bash` for Node — gone forever

One file. All versions pinned. All tools cached as binaries. 19 minutes down to 5.

[PR #631](https://github.com/security-union/videocall-rs/pull/631) has the full diff if you want to steal this for your own project.
