+++
title = "Delete Your C Dependencies. AI Will Rewrite Them."
date = 2026-06-30
draft = false
slug = "delete-your-c-dependencies"
description = "We replaced native libopus in videocall.rs with ropus, an AI-generated pure-Rust Opus codec. Why AI-regenerated native libraries will end our dependence on C shared libraries."
[extra]
seo_keywords = ["libopus", "ropus", "rust", "ai generated code", "shared libraries", "ffi", "wasm", "opus codec", "audiopus", "linux dependencies"]
[taxonomies]
tags = ["rust", "ai", "codecs", "opus", "wasm", "dependencies", "ffi", "build-systems"]
authors = ["Dario Lencina Talarico"]
+++

libopus broke my build again. Different Ubuntu in CI. pkg-config missing on a Mac. cmake choking on a cross-compile. And it never builds for `wasm32-unknown-unknown` at all — there's no C runtime to link against. I added `apt install` to a Dockerfile and lived with it for a year. Last week I deleted it.

## The problem with shared libraries

A shared library is a binary you didn't build, at a path you don't control, at a version your distro picked, behind an FFI boundary the borrow checker can't see. To call libopus from Rust you add a `-sys` crate, a C toolchain, cmake, and pkg-config. The `-sys` crate is unmaintained, because maintaining FFI glue is thankless work nobody signs up for. It sits on the critical path of the whole product.

The failure modes are operational: glibc version skew, `libfoo.so.3` against `.so.4`, works-in-apt-breaks-in-nix, an unaudited binary in your supply chain, and no browser target. A pure-Rust project with a C dependency is a C project.

We accepted this because hand-porting a codec costs months and ships bugs the original doesn't have. Opus is SILK, CELT, range coding, and fifteen years of edge cases pinned by conformance vectors. That cost is what kept the `.so` in place.

## What we did

We replaced libopus with [`ropus`](https://github.com/0x4D44/ropus), a pure-Rust port of xiph Opus. One runtime dependency: `wide`, for portable SIMD. No C, no cmake, no `-sys`. It is bit-exact against the C reference and passes all seven of xiph's conformance binaries plus the RFC 6716 vectors. BSD-3, the same license as libopus.

The change was small. Swap the decoder in our jitter buffer, swap the encoder in the CLI and the load-test bot, delete `opus` from every `Cargo.toml`. `opus` and `audiopus-sys` left `Cargo.lock`. The C toolchain and the cmake step left with them.

Then I streamed a real call from a USB webcam mic: 48 kHz stereo in, down-mixed to mono, 50 Opus frames per second, encoded in Rust, decoded by a browser. No C in the audio path.

## ropus is AI-generated

An AI read the C and wrote the Rust, then ground its output against the conformance vectors until it matched the C reference byte for byte. The result is a bit-exact reimplementation of one of the most deployed audio codecs in the world, in a memory-safe language, passing the spec's own torture tests.

Codecs are the ideal target. Correctness is defined by a published test suite. You check the port against those vectors. They are the authority, and they are fifteen years old.

## Why this is a trend

The shared library is dying, and AI is what kills it.

Every `libfoo.so` we kept was a cost-of-rewriting decision. Reimplementing libvpx, libwebp, libpng, or zlib by hand cost months of expert time at high risk. That price dropped to days, with correctness enforced by the upstream test suite.

The procedure is mechanical. Generate a memory-safe native port. Pin it to the upstream conformance suite. Delete the FFI. `-sys` crates leave the lockfile. `apt install` leaves the Dockerfile. The project compiles to wasm. The supply chain shrinks to source you can read. The unsafe FFI boundary is gone.

## Where this breaks

This needs a conformance suite. Opus has one; most of the codec, crypto, and compression world does too. Without a test corpus pinning correctness, an AI port is unverifiable — skip it. Performance has to land: ropus runs at roughly C parity, and a port that's 5x slower is a toy, so measure before you ship. The trust anchor is always the upstream vectors. The model just grinds until they pass.

## Credit

ropus is the work of **Martin Davidson ([@0x4D44](https://github.com/0x4D44/ropus))**. Pointing a model at libopus and getting Rust out is the easy part. The hard part is proving the Rust is correct: a differential FFI harness, all seven xiph conformance binaries building against a C-ABI shim and passing, the RFC 6716 vectors pinned, fuzz campaigns, and journals documenting every decision. The model is a power tool. Martin is the engineer who knew what to build with it. Star the repo.

## Takeaway

There is no native Opus library in videocall.rs anymore — not in the crates, the lockfile, the Nix shell, or the Docker image. The dependency that broke my builds for a year is gone.

Shared libraries were a workaround for expensive rewrites. The rewrite is cheap now.

Open your `Cargo.lock`. Count the `-sys` crates. That's the list.

— Dario
