+++
title = "Delete Your C Dependencies. AI Will Rewrite Them."
date = 2026-06-30
draft = false
slug = "delete-your-c-dependencies"
description = "We ripped native libopus out of videocall.rs and replaced it with ropus, an AI-generated pure-Rust Opus codec. The build got simpler, wasm started working, and I think this is the beginning of the end for the shared library."
[extra]
seo_keywords = ["libopus", "ropus", "rust", "ai generated code", "shared libraries", "ffi", "wasm", "opus codec", "audiopus", "linux dependencies"]
[taxonomies]
tags = ["rust", "ai", "codecs", "opus", "wasm", "dependencies", "ffi", "build-systems"]
authors = ["Dario Lencina Talarico"]
+++

The build broke again. Not my code. libopus.

If you've shipped anything that touches audio, you know the dance. You add `audiopus-sys`, or the `opus` crate, or whatever -sys crate wraps the C library this week. It works on your machine. Then CI is on a different Ubuntu. Then someone builds on a Mac and pkg-config can't find it. Then you cross-compile and cmake throws up. Then you try to build for the browser and just... stop, because libopus is a C library and `wasm32-unknown-unknown` doesn't have a C runtime to link against.

This is the part where most people add `apt install` to a Dockerfile and move on. I did that for a year. Then I deleted the whole thing.

## The actual problem

libopus is great software. That's not the point. The point is that *the way we consume it* is a 1995 idea wearing a 2026 hat.

A shared library is a binary blob you don't build, sitting at a path you don't control, at a version your distro picked, exposed through an FFI boundary your borrow checker can't see. You write `unsafe` and pray the struct layout matches. Your "Rust" project is now a Rust project plus a C toolchain plus cmake plus pkg-config plus a `-sys` crate that was last published when people still cared about Python 2.

And the `-sys` crate is *always* unmaintained. Always. Because nobody's career was ever made maintaining FFI glue. It's the most thankless code in the ecosystem and it sits on the critical path of your entire product.

The cost isn't theoretical. It's glibc skew. It's `libfoo.so.3` vs `.so.4`. It's "works in apt, breaks in nix, who knows in Alpine." It's the supply-chain surface of a binary you didn't compile. It's the fact that your beautiful pure-Rust, memory-safe, runs-anywhere project has a soft underbelly of C that makes "runs anywhere" a lie. We don't run in the browser, because C doesn't.

We tolerated all of this for one reason: **rewriting a codec by hand is months of work, and you'll introduce bugs the original didn't have.** Opus is SILK plus CELT plus range coding plus a decade of edge cases pinned down by conformance vectors. Nobody sane volunteers for that.

That reason is now gone.

## What we did

We replaced native libopus with [`ropus`](https://github.com/0x4D44/ropus): a pure-Rust port of the xiph Opus codec. No C. No cmake. No `-sys`. One runtime dependency (`wide`, for portable SIMD). It is bit-exact against the C reference — it passes all seven of xiph's own conformance binaries and the RFC 6716 test vectors. It's BSD-3, same license as libopus, so there's nothing new to argue with legal about.

The diff across videocall.rs was almost boring. Swap the decoder backend in our jitter buffer. Swap the encoder in the CLI and the load-test bot. Delete `opus` from every `Cargo.toml`. Watch `opus` and `audiopus-sys` disappear from `Cargo.lock`. The C toolchain requirement — gone. The cmake-for-opus step — gone.

Then I plugged in an external USB webcam mic and streamed a real call. 48 kHz stereo in, down-mixed to mono, 50 Opus frames a second, encoded in pure Rust, decoded clean by a browser on the other end. Zero C in the audio path. It just worked.

And here's the part that should make you sit up.

## ropus is AI-generated

A human didn't sit down and hand-port libopus. An AI read the C and wrote the Rust. Then it checked its own work against the original's conformance vectors until the output matched the C reference byte for byte.

Read that again. **An AI produced a bit-exact reimplementation of one of the most widely deployed audio codecs on Earth, in a memory-safe language, that passes the spec's own torture tests.**

This is not autocomplete. This is not "it wrote a function." This is a full, verifiable replacement of a load-bearing C library, and the proof that it's correct isn't vibes — it's that the bits coming out are identical to the bits the C produces. Codecs are the perfect target for this, because correctness is *defined* by a test suite. You don't have to trust the AI. You have to trust the conformance vectors, and those have been around for fifteen years.

## Why this is a trend, not a one-off

Here's my prediction, and I'll be blunt about it: **the shared library is dying, and AI is what kills it.**

Every reason we put up with `libfoo.so` was a cost-of-rewriting argument. "We can't reimplement libvpx / libopus / libwebp / libpng / zlib, it's too much work, it's too easy to get wrong." That argument had a price tag of months-to-years of expert time. The price tag just dropped to days, and the correctness bar is enforced by the original library's own tests.

So what happens? You stop linking C. You generate a native, memory-safe port, you pin it against the upstream conformance suite, and you delete the FFI. Your `Cargo.lock` stops having `-sys` crates in it. Your Dockerfile stops having `apt install`. Your project compiles to wasm because there's no C left to not-compile. Your supply chain shrinks to source you can actually read. The unsafe FFI boundary — the thing the borrow checker was helpless against — is just *gone*.

This isn't "AI writes new code." Everyone's bored of that take. This is **AI deletes old code** — specifically, it deletes the gnarliest, least-loved, most-load-bearing C in your stack, and hands you back something that runs everywhere and passes the same tests.

## The honest caveats

I'm cocky, not dishonest. A few things have to be true for this to work, and they were true here:

- **There has to be a conformance suite.** Opus has one. So does most of the codec/crypto/compression world. If correctness is defined by a test corpus, AI porting is verifiable. If it's defined by "looks right," don't.
- **Performance has to land.** ropus runs roughly at parity with C on encode and decode. A pure port that's 5x slower is a toy. Check this, don't assume it.
- **You verify against the original, not the AI.** The trust anchor is the spec's vectors. The AI is just the thing that grinds until the vectors pass.

## Respect to the author

ropus is the work of **Martin Davidson ([@0x4D44](https://github.com/0x4D44/ropus))**, and I want to be loud about this, because the AI angle buries the lede on the human one. Pointing a model at libopus and getting Rust out is the easy 20%. Building the thing that *proves* the Rust is bit-exact is the other 80%: a differential FFI harness, all seven of xiph's conformance binaries compiling against a C-ABI shim and passing, the RFC 6716 vectors pinned, fuzz campaigns, and engineering journals documenting every decision. That's the unglamorous, load-bearing work, and it's exactly what turns "an AI wrote a codec" from a scary tweet into something you'd put in production. The model is a power tool. Martin is the engineer who knew what to build with it. Go star the repo.

## The takeaway

We didn't patch our libopus problem. We deleted the category. There is no native Opus library in videocall.rs anymore — not in the crates, not in the lockfile, not in the Nix shell, not in the Docker image. The thing that broke my builds for a year is not pinned to a newer version. It's *not there.*

Shared library dependencies were never a feature. They were a workaround for the fact that rewriting good C was expensive. It isn't anymore.

Go look at your `Cargo.lock`. Count the `-sys` crates. That's your to-delete list.

— Dario
