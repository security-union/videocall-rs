/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Regenerate `videocall-codecs/src/vp9/common/generated.rs` from a libvpx
//! source checkout.
//!
//! ```text
//! LIBVPX_SRC=~/Documents/libvpx cargo run -p videocall-codecs --example extract_vp9_tables
//! ```
//!
//! `LIBVPX_SRC` defaults to `~/Documents/libvpx`. The generator is idempotent:
//! running it against an unchanged checkout leaves the file byte-identical.

use std::path::{Path, PathBuf};

#[path = "../src/vp9/table_extract.rs"]
mod table_extract;

fn libvpx_src() -> PathBuf {
    if let Ok(p) = std::env::var("LIBVPX_SRC") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME not set and LIBVPX_SRC unset");
    Path::new(&home).join("Documents/libvpx")
}

fn main() {
    let src = libvpx_src();
    assert!(
        src.join("vp9/common/vp9_entropy.c").exists(),
        "libvpx checkout not found at {} (set LIBVPX_SRC)",
        src.display()
    );

    let content = table_extract::generate(&src);

    // This example lives at <crate>/examples/, so the crate root is its parent.
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = crate_root.join("src/vp9/common/generated.rs");
    std::fs::write(&out, &content).expect("failed to write generated.rs");
    eprintln!(
        "wrote {} ({} bytes) from libvpx at {}",
        out.display(),
        content.len(),
        src.display()
    );
}
