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

//! VP9 constant-table extractor (build-time tooling, std-only).
//!
//! This file is intentionally NOT a module of the `videocall-codecs` crate. It
//! is `#[path]`-included by both `examples/extract_vp9_tables.rs` (which writes
//! `src/vp9/common/generated.rs`) and the ignored verification test
//! `generated_tables_match_libvpx_source`, so the two share one implementation
//! without pulling filesystem/tooling code into the shipped library or its
//! wasm build.
//!
//! [`generate`] reads a libvpx source checkout and emits the full text of
//! `generated.rs`. It is deterministic (idempotent): running it twice against
//! the same checkout produces byte-identical output.

#![allow(dead_code)]

use std::path::Path;

/// Read `<libvpx>/<rel>` or panic with a helpful message.
fn read(libvpx: &Path, rel: &str) -> String {
    let p = libvpx.join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

/// Strip C `//` and `/* */` comments, honouring string and char literals so a
/// `/` inside a string can't be mistaken for a comment start.
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' | b'\'' => {
                let quote = b[i];
                out.push(b[i] as char);
                i += 1;
                while i < b.len() {
                    out.push(b[i] as char);
                    if b[i] == b'\\' && i + 1 < b.len() {
                        out.push(b[i + 1] as char);
                        i += 2;
                        continue;
                    }
                    if b[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Locate the brace-balanced initializer body of a top-level array definition
/// named `name` (the text between and including the outermost `{ }`). Requires
/// `name[` … `= {` and rejects mere references/usages of the symbol.
fn find_init<'a>(src: &'a str, name: &str) -> &'a str {
    let bytes = src.as_bytes();
    let mut search = 0;
    while let Some(rel) = src[search..].find(name) {
        let start = search + rel;
        let end = start + name.len();
        search = end;
        // Word isolation: preceding char must not be an identifier char. The
        // following char must break the identifier and introduce a definition:
        // '[' (array dims), '=' (struct), or whitespace before either.
        let prev_ok = start == 0 || !is_ident(bytes[start - 1]);
        let next_ok =
            end < bytes.len() && matches!(bytes[end], b'[' | b'=' | b' ' | b'\t' | b'\n' | b'\r');
        if !(prev_ok && next_ok) {
            continue;
        }
        // Between the name and the '=' we expect only "[dim]" specs — no ';'/'{'.
        let after = &src[end..];
        let Some(eq) = after.find('=') else { continue };
        if after[..eq].contains(';') || after[..eq].contains('{') {
            continue;
        }
        let rest = &after[eq + 1..];
        let Some(brace) = rest.find('{') else {
            continue;
        };
        if rest[..brace].contains(';') {
            continue;
        }
        return balanced(&rest[brace..]);
    }
    panic!("array definition `{name}` not found");
}

fn is_ident(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// Return the slice starting at the leading `{` up to and including its matching
/// `}`.
fn balanced(s: &str) -> &str {
    let b = s.as_bytes();
    assert_eq!(b[0], b'{');
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &s[..=i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces");
}

/// Pull every integer literal (in order) from a comment-stripped brace body.
/// The bodies of interest contain only numbers, braces, commas and whitespace.
fn ints(body: &str) -> Vec<i64> {
    let mut cleaned = String::with_capacity(body.len());
    for c in body.chars() {
        if c.is_ascii_digit() || c == '-' {
            cleaned.push(c);
        } else {
            cleaned.push(' ');
        }
    }
    cleaned
        .split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect()
}

/// Extract the integer literals of array `name` from `src`.
fn array(src: &str, name: &str) -> Vec<i64> {
    ints(find_init(src, name))
}

// ---------------------------------------------------------------------------
// Rust source emitters
// ---------------------------------------------------------------------------

/// Format a value list nested according to `dims` (row-major), e.g. dims
/// `[2, 3]` groups 6 values into `[[a, b, c], [d, e, f]]`.
fn nested(vals: &[i64], dims: &[usize]) -> String {
    if dims.len() == 1 {
        assert_eq!(
            vals.len(),
            dims[0],
            "flat len {} != {}",
            vals.len(),
            dims[0]
        );
        let items: Vec<String> = vals.iter().map(|v| v.to_string()).collect();
        return format!("[{}]", items.join(", "));
    }
    let inner: usize = dims[1..].iter().product();
    assert_eq!(vals.len(), dims[0] * inner, "shape mismatch for {dims:?}");
    let groups: Vec<String> = vals.chunks(inner).map(|c| nested(c, &dims[1..])).collect();
    format!("[{}]", groups.join(", "))
}

fn type_str(elem: &str, dims: &[usize]) -> String {
    let mut t = elem.to_string();
    for &d in dims.iter().rev() {
        t = format!("[{t}; {d}]");
    }
    t
}

/// Emit `pub const NAME: TYPE = VALUES;` for a table of shape `dims`. Each const
/// carries `#[rustfmt::skip]` (a stable item attribute) so `cargo fmt` leaves the
/// compact generated layout byte-identical to the extractor output.
fn emit(name: &str, elem: &str, dims: &[usize], vals: &[i64]) -> String {
    format!(
        "#[rustfmt::skip]\npub const {}: {} = {};\n\n",
        name,
        type_str(elem, dims),
        nested(vals, dims)
    )
}

// ---------------------------------------------------------------------------
// Coefficient-probability densification
// ---------------------------------------------------------------------------

/// libvpx `vp9_coeff_probs_model` is a dense `[REF][BAND][CTX=6][NODE=3]` type,
/// but band 0's initializer lists only 3 of the 6 contexts (the rest are
/// C zero-filled). The literal order is plane-major:
/// `for plane { for ref { band0(3 ctx) band1..5(6 ctx) } }`. Expand the flat
/// literal into the dense `[2][2][6][6][3]` layout with band-0 padding.
fn densify_coef_probs(flat: &[i64]) -> Vec<i64> {
    const PLANES: usize = 2;
    const REFS: usize = 2;
    const BANDS: usize = 6;
    const CTX: usize = 6;
    const NODES: usize = 3;
    let mut out = Vec::with_capacity(PLANES * REFS * BANDS * CTX * NODES);
    let mut c = 0usize;
    for _plane in 0..PLANES {
        for _r in 0..REFS {
            for band in 0..BANDS {
                let ctx_present = if band == 0 { 3 } else { CTX };
                for _ctx in 0..ctx_present {
                    for _n in 0..NODES {
                        out.push(flat[c]);
                        c += 1;
                    }
                }
                // Zero-pad the missing band-0 contexts.
                out.resize(out.len() + (CTX - ctx_present) * NODES, 0);
            }
        }
    }
    assert_eq!(
        c,
        flat.len(),
        "consumed {c} of {} coef prob ints",
        flat.len()
    );
    out
}

// ---------------------------------------------------------------------------
// Top-level generation
// ---------------------------------------------------------------------------

/// Generate the full text of `src/vp9/common/generated.rs` from a libvpx
/// checkout rooted at `libvpx`.
pub fn generate(libvpx: &Path) -> String {
    let quant = strip_comments(&read(libvpx, "vp9/common/vp9_quant_common.c"));
    let entropy = strip_comments(&read(libvpx, "vp9/common/vp9_entropy.c"));
    let mode = strip_comments(&read(libvpx, "vp9/common/vp9_entropymode.c"));
    let mv = strip_comments(&read(libvpx, "vp9/common/vp9_entropymv.c"));
    let scan = strip_comments(&read(libvpx, "vp9/common/vp9_scan.c"));

    let mut o = String::new();
    o.push_str(HEADER);

    // --- Quantizer lookups (8-bit only) -----------------------------------
    o.push_str("// Dequant lookups (`vp9/common/vp9_quant_common.c`, 8-bit).\n");
    o.push_str(&emit(
        "DC_QLOOKUP",
        "i16",
        &[256],
        &array(&quant, "dc_qlookup"),
    ));
    o.push_str(&emit(
        "AC_QLOOKUP",
        "i16",
        &[256],
        &array(&quant, "ac_qlookup"),
    ));

    // --- Coefficient bands / energy / pareto ------------------------------
    o.push_str("// Coefficient band + energy tables (`vp9/common/vp9_entropy.c`).\n");
    o.push_str(&emit(
        "COEFBAND_TRANS_4X4",
        "u8",
        &[16],
        &array(&entropy, "vp9_coefband_trans_4x4"),
    ));
    o.push_str(&emit(
        "COEFBAND_TRANS_8X8PLUS",
        "u8",
        &[1024],
        &array(&entropy, "vp9_coefband_trans_8x8plus"),
    ));
    o.push_str(&emit(
        "PT_ENERGY_CLASS",
        "u8",
        &[12],
        &array(&entropy, "vp9_pt_energy_class"),
    ));
    o.push_str("// Pareto-8 model tail probabilities (`vp9_pareto8_full[255][8]`).\n");
    o.push_str(&emit(
        "PARETO8_FULL",
        "u8",
        &[255, 8],
        &array(&entropy, "vp9_pareto8_full"),
    ));

    // --- CAT extra-bit probabilities --------------------------------------
    o.push_str("// Coefficient category extra-bit probabilities (`vp9_cat{1..6}_prob`).\n");
    o.push_str(&emit(
        "CAT1_PROB",
        "u8",
        &[1],
        &array(&entropy, "vp9_cat1_prob"),
    ));
    o.push_str(&emit(
        "CAT2_PROB",
        "u8",
        &[2],
        &array(&entropy, "vp9_cat2_prob"),
    ));
    o.push_str(&emit(
        "CAT3_PROB",
        "u8",
        &[3],
        &array(&entropy, "vp9_cat3_prob"),
    ));
    o.push_str(&emit(
        "CAT4_PROB",
        "u8",
        &[4],
        &array(&entropy, "vp9_cat4_prob"),
    ));
    o.push_str(&emit(
        "CAT5_PROB",
        "u8",
        &[5],
        &array(&entropy, "vp9_cat5_prob"),
    ));
    o.push_str(&emit(
        "CAT6_PROB",
        "u8",
        &[14],
        &array(&entropy, "vp9_cat6_prob"),
    ));

    // --- Default coefficient probabilities (densified) --------------------
    o.push_str(
        "// Default coefficient model probabilities, dense [PLANE][REF][BAND][CTX][NODE]\n\
         // (`vp9/common/vp9_entropy.c`; band 0's 3 unused contexts are zero-filled).\n",
    );
    for (name, sym) in [
        ("DEFAULT_COEF_PROBS_4X4", "default_coef_probs_4x4"),
        ("DEFAULT_COEF_PROBS_8X8", "default_coef_probs_8x8"),
        ("DEFAULT_COEF_PROBS_16X16", "default_coef_probs_16x16"),
        ("DEFAULT_COEF_PROBS_32X32", "default_coef_probs_32x32"),
    ] {
        let dense = densify_coef_probs(&array(&entropy, sym));
        o.push_str(&emit(name, "u8", &[2, 2, 6, 6, 3], &dense));
    }

    // --- Intra / keyframe mode probabilities ------------------------------
    o.push_str("// Keyframe + inter-frame intra mode probabilities (`vp9_entropymode.c`).\n");
    o.push_str(&emit(
        "KF_Y_MODE_PROBS",
        "u8",
        &[10, 10, 9],
        &array(&mode, "vp9_kf_y_mode_prob"),
    ));
    o.push_str(&emit(
        "KF_UV_MODE_PROBS",
        "u8",
        &[10, 9],
        &array(&mode, "vp9_kf_uv_mode_prob"),
    ));
    o.push_str(&emit(
        "DEFAULT_IF_Y_PROBS",
        "u8",
        &[4, 9],
        &array(&mode, "default_if_y_probs"),
    ));
    o.push_str(&emit(
        "DEFAULT_IF_UV_PROBS",
        "u8",
        &[10, 9],
        &array(&mode, "default_if_uv_probs"),
    ));

    // --- Partition probabilities ------------------------------------------
    o.push_str("// Partition probabilities (`vp9_entropymode.c`).\n");
    o.push_str(&emit(
        "KF_PARTITION_PROBS",
        "u8",
        &[16, 3],
        &array(&mode, "vp9_kf_partition_probs"),
    ));
    o.push_str(&emit(
        "DEFAULT_PARTITION_PROBS",
        "u8",
        &[16, 3],
        &array(&mode, "default_partition_probs"),
    ));

    // --- Misc mode / reference probabilities ------------------------------
    o.push_str("// Skip / inter-mode / reference probabilities (`vp9_entropymode.c`).\n");
    o.push_str(&emit(
        "DEFAULT_SKIP_PROBS",
        "u8",
        &[3],
        &array(&mode, "default_skip_probs"),
    ));
    o.push_str(&emit(
        "DEFAULT_INTRA_INTER_PROBS",
        "u8",
        &[4],
        &array(&mode, "default_intra_inter_p"),
    ));
    o.push_str(&emit(
        "DEFAULT_COMP_INTER_PROBS",
        "u8",
        &[5],
        &array(&mode, "default_comp_inter_p"),
    ));
    o.push_str(&emit(
        "DEFAULT_COMP_REF_PROBS",
        "u8",
        &[5],
        &array(&mode, "default_comp_ref_p"),
    ));
    o.push_str(&emit(
        "DEFAULT_SINGLE_REF_PROBS",
        "u8",
        &[5, 2],
        &array(&mode, "default_single_ref_p"),
    ));
    o.push_str(&emit(
        "DEFAULT_INTER_MODE_PROBS",
        "u8",
        &[7, 3],
        &array(&mode, "default_inter_mode_probs"),
    ));
    o.push_str(&emit(
        "DEFAULT_SWITCHABLE_INTERP_PROBS",
        "u8",
        &[4, 2],
        &array(&mode, "default_switchable_interp_prob"),
    ));

    // --- Transform-size probabilities (irregular struct) ------------------
    // default_tx_probs = { p32x32[2][3], p16x16[2][2], p8x8[2][1] }.
    o.push_str("// Transform-size probabilities (`default_tx_probs`, `vp9_entropymode.c`).\n");
    let tx = array(&mode, "default_tx_probs");
    assert_eq!(tx.len(), 12, "tx probs len");
    o.push_str(&emit("DEFAULT_TX_PROBS_32X32", "u8", &[2, 3], &tx[0..6]));
    o.push_str(&emit("DEFAULT_TX_PROBS_16X16", "u8", &[2, 2], &tx[6..10]));
    o.push_str(&emit("DEFAULT_TX_PROBS_8X8", "u8", &[2, 1], &tx[10..12]));

    // --- Default NMV context (struct → flat 69) ---------------------------
    // joints[3] then per component (vert, horiz): sign, class[10], class0[1],
    // bits[10], class0_fp[2][3], fp[3], class0_hp, hp.
    o.push_str("// Default motion-vector entropy context (`default_nmv_context`).\n");
    let nmv = array(&mv, "default_nmv_context");
    assert_eq!(nmv.len(), 69, "nmv context len");
    let mut c = 0usize;
    let take = |n: usize, cur: &mut usize| {
        let s = nmv[*cur..*cur + n].to_vec();
        *cur += n;
        s
    };
    let joints = take(3, &mut c);
    let mut sign = Vec::new();
    let mut classes = Vec::new();
    let mut class0 = Vec::new();
    let mut bits = Vec::new();
    let mut class0_fp = Vec::new();
    let mut fp = Vec::new();
    let mut class0_hp = Vec::new();
    let mut hp = Vec::new();
    for _comp in 0..2 {
        sign.extend(take(1, &mut c));
        classes.extend(take(10, &mut c));
        class0.extend(take(1, &mut c));
        bits.extend(take(10, &mut c));
        class0_fp.extend(take(6, &mut c));
        fp.extend(take(3, &mut c));
        class0_hp.extend(take(1, &mut c));
        hp.extend(take(1, &mut c));
    }
    assert_eq!(c, 69);
    o.push_str(&emit("NMV_JOINT_PROBS", "u8", &[3], &joints));
    o.push_str(&emit("NMV_SIGN_PROBS", "u8", &[2], &sign));
    o.push_str(&emit("NMV_CLASS_PROBS", "u8", &[2, 10], &classes));
    o.push_str(&emit("NMV_CLASS0_PROBS", "u8", &[2, 1], &class0));
    o.push_str(&emit("NMV_BITS_PROBS", "u8", &[2, 10], &bits));
    o.push_str(&emit("NMV_CLASS0_FP_PROBS", "u8", &[2, 2, 3], &class0_fp));
    o.push_str(&emit("NMV_FP_PROBS", "u8", &[2, 3], &fp));
    o.push_str(&emit("NMV_CLASS0_HP_PROBS", "u8", &[2], &class0_hp));
    o.push_str(&emit("NMV_HP_PROBS", "u8", &[2], &hp));

    // --- Scan orders + neighbor arrays ------------------------------------
    o.push_str("// Scan orders (`vp9/common/vp9_scan.c`). Sizes n*n; neighbors (n*n+1)*2.\n");
    let scans: [(&str, &str, usize); 10] = [
        ("SCAN_DEFAULT_4X4", "default_scan_4x4", 16),
        ("SCAN_COL_4X4", "col_scan_4x4", 16),
        ("SCAN_ROW_4X4", "row_scan_4x4", 16),
        ("SCAN_DEFAULT_8X8", "default_scan_8x8", 64),
        ("SCAN_COL_8X8", "col_scan_8x8", 64),
        ("SCAN_ROW_8X8", "row_scan_8x8", 64),
        ("SCAN_DEFAULT_16X16", "default_scan_16x16", 256),
        ("SCAN_COL_16X16", "col_scan_16x16", 256),
        ("SCAN_ROW_16X16", "row_scan_16x16", 256),
        ("SCAN_DEFAULT_32X32", "default_scan_32x32", 1024),
    ];
    for (name, sym, n) in scans {
        o.push_str(&emit(name, "i16", &[n], &array(&scan, sym)));
    }
    o.push_str("// Scan neighbor arrays (`vp9/common/vp9_scan.c`).\n");
    let neigh: [(&str, &str, usize); 10] = [
        (
            "NEIGHBORS_DEFAULT_4X4",
            "default_scan_4x4_neighbors",
            17 * 2,
        ),
        ("NEIGHBORS_COL_4X4", "col_scan_4x4_neighbors", 17 * 2),
        ("NEIGHBORS_ROW_4X4", "row_scan_4x4_neighbors", 17 * 2),
        (
            "NEIGHBORS_DEFAULT_8X8",
            "default_scan_8x8_neighbors",
            65 * 2,
        ),
        ("NEIGHBORS_COL_8X8", "col_scan_8x8_neighbors", 65 * 2),
        ("NEIGHBORS_ROW_8X8", "row_scan_8x8_neighbors", 65 * 2),
        (
            "NEIGHBORS_DEFAULT_16X16",
            "default_scan_16x16_neighbors",
            257 * 2,
        ),
        ("NEIGHBORS_COL_16X16", "col_scan_16x16_neighbors", 257 * 2),
        ("NEIGHBORS_ROW_16X16", "row_scan_16x16_neighbors", 257 * 2),
        (
            "NEIGHBORS_DEFAULT_32X32",
            "default_scan_32x32_neighbors",
            1025 * 2,
        ),
    ];
    for (name, sym, n) in neigh {
        o.push_str(&emit(name, "i16", &[n], &array(&scan, sym)));
    }

    // Match rustfmt's canonical EOF: exactly one trailing newline, no blank line.
    let mut o = o.trim_end().to_string();
    o.push('\n');
    o
}

const HEADER: &str = "\
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

//! VP9 default probability and constant tables.
//!
//! GENERATED FILE — do not edit by hand. Regenerate with:
//!
//! ```text
//! LIBVPX_SRC=~/Documents/libvpx cargo run -p videocall-codecs --example extract_vp9_tables
//! ```
//!
//! Every table is transcribed verbatim from the libvpx C sources by
//! `src/vp9/table_extract.rs`; see that file for the source array names and
//! reshaping rules. The `generated_tables_match_libvpx_source` test verifies
//! this file still matches the checkout.

#![allow(clippy::all)]
#![allow(dead_code)] // consumed in later milestones

";
