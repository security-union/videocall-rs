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

//! Tests for the generated VP9 tables: hand-verified spot checks (always run)
//! plus an `#[ignore]`d re-extraction diff against a libvpx checkout.

use super::generated::*;

// Spot checks with values verified by hand against the libvpx C sources.

#[test]
fn dc_ac_qlookup_endpoints() {
    // vp9/common/vp9_quant_common.c, 8-bit dc_qlookup / ac_qlookup.
    assert_eq!(DC_QLOOKUP[0], 4);
    assert_eq!(DC_QLOOKUP[255], 1336);
    assert_eq!(AC_QLOOKUP[0], 4);
    assert_eq!(AC_QLOOKUP[255], 1828);
    assert_eq!(DC_QLOOKUP.len(), 256);
    assert_eq!(AC_QLOOKUP.len(), 256);
}

#[test]
fn pareto8_first_row() {
    // vp9/common/vp9_entropy.c: first row of vp9_pareto8_full.
    assert_eq!(PARETO8_FULL[0], [3, 86, 128, 6, 86, 23, 88, 29]);
    assert_eq!(PARETO8_FULL.len(), 255);
}

#[test]
fn coefband_and_energy() {
    assert_eq!(
        COEFBAND_TRANS_4X4,
        [0, 1, 1, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 5, 5, 5]
    );
    assert_eq!(PT_ENERGY_CLASS, [0, 1, 2, 3, 3, 4, 4, 5, 5, 5, 5, 5]);
    assert_eq!(COEFBAND_TRANS_8X8PLUS.len(), 1024);
}

#[test]
fn cat_probs() {
    // vp9/common/vp9_entropy.c: vp9_cat{1..6}_prob.
    assert_eq!(CAT1_PROB, [159]);
    assert_eq!(CAT2_PROB, [165, 145]);
    assert_eq!(
        CAT6_PROB,
        [254, 254, 254, 252, 249, 243, 230, 196, 177, 153, 140, 133, 130, 129]
    );
}

#[test]
fn kf_y_mode_first_rows() {
    // vp9/common/vp9_entropymode.c: vp9_kf_y_mode_prob[dc][dc] and [dc][v].
    assert_eq!(
        KF_Y_MODE_PROBS[0][0],
        [137, 30, 42, 148, 151, 207, 70, 52, 91]
    );
    assert_eq!(
        KF_Y_MODE_PROBS[0][1],
        [92, 45, 102, 136, 116, 180, 74, 90, 100]
    );
    assert_eq!(
        KF_UV_MODE_PROBS[0],
        [144, 11, 54, 157, 195, 130, 46, 58, 108]
    );
}

#[test]
fn default_probs_shapes_and_values() {
    // Hand-verified from vp9/common/vp9_entropymode.c.
    assert_eq!(DEFAULT_SKIP_PROBS, [192, 128, 64]);
    assert_eq!(DEFAULT_INTRA_INTER_PROBS, [9, 102, 187, 225]);
    assert_eq!(DEFAULT_SINGLE_REF_PROBS[0], [33, 16]);
    assert_eq!(DEFAULT_SINGLE_REF_PROBS[4], [238, 247]);
    // default_tx_probs = { p32x32, p16x16, p8x8 }.
    assert_eq!(DEFAULT_TX_PROBS_32X32, [[3, 136, 37], [5, 52, 13]]);
    assert_eq!(DEFAULT_TX_PROBS_16X16, [[20, 152], [15, 101]]);
    assert_eq!(DEFAULT_TX_PROBS_8X8, [[100], [66]]);
}

#[test]
fn coef_probs_band0_padding_and_first_row() {
    // Dense [PLANE][REF][BAND][CTX][NODE]; first literal row of 4x4 Y/intra.
    assert_eq!(DEFAULT_COEF_PROBS_4X4[0][0][0][0], [195, 29, 183]);
    // Band 0 has only 3 real contexts; contexts 3..6 must be zero-filled.
    assert_eq!(DEFAULT_COEF_PROBS_4X4[0][0][0][3], [0, 0, 0]);
    assert_eq!(DEFAULT_COEF_PROBS_4X4[0][0][0][5], [0, 0, 0]);
    // Band 1 context 0 is populated.
    assert_eq!(DEFAULT_COEF_PROBS_4X4[0][0][1][0], [31, 107, 169]);
}

#[test]
fn nmv_context_values() {
    // vp9/common/vp9_entropymv.c: default_nmv_context.
    assert_eq!(NMV_JOINT_PROBS, [32, 64, 96]);
    // Vertical component.
    assert_eq!(NMV_SIGN_PROBS[0], 128);
    assert_eq!(
        NMV_CLASS_PROBS[0],
        [224, 144, 192, 168, 192, 176, 192, 198, 198, 245]
    );
    assert_eq!(NMV_CLASS0_PROBS[0], [216]);
    assert_eq!(NMV_CLASS0_FP_PROBS[0], [[128, 128, 64], [96, 112, 64]]);
    assert_eq!(NMV_FP_PROBS[0], [64, 96, 64]);
    // Horizontal component classes differ.
    assert_eq!(
        NMV_CLASS_PROBS[1],
        [216, 128, 176, 160, 176, 176, 192, 198, 198, 208]
    );
    assert_eq!(NMV_CLASS0_PROBS[1], [208]);
}

#[test]
fn scan_orders_first_entries() {
    // vp9/common/vp9_scan.c: scans start at DC (0); default_scan_4x4[1] == 4.
    assert_eq!(SCAN_DEFAULT_4X4[0], 0);
    assert_eq!(SCAN_DEFAULT_4X4[1], 4);
    assert_eq!(SCAN_DEFAULT_4X4.len(), 16);
    assert_eq!(SCAN_DEFAULT_8X8.len(), 64);
    assert_eq!(SCAN_DEFAULT_16X16.len(), 256);
    assert_eq!(SCAN_DEFAULT_32X32.len(), 1024);
    assert_eq!(NEIGHBORS_DEFAULT_4X4.len(), 34);
    assert_eq!(NEIGHBORS_DEFAULT_32X32.len(), 2050);
}

// Re-extraction diff test. Skips cleanly when no libvpx checkout is available.

#[path = "../table_extract.rs"]
mod table_extract;

#[test]
#[ignore = "requires LIBVPX_SRC checkout"]
fn generated_tables_match_libvpx_source() {
    use std::path::PathBuf;

    let libvpx: PathBuf = match std::env::var("LIBVPX_SRC") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let Ok(home) = std::env::var("HOME") else {
                eprintln!("skipping: LIBVPX_SRC and HOME both unset");
                return;
            };
            PathBuf::from(home).join("Documents/libvpx")
        }
    };
    if !libvpx.join("vp9/common/vp9_entropy.c").exists() {
        eprintln!(
            "skipping generated_tables_match_libvpx_source: no libvpx checkout at {}",
            libvpx.display()
        );
        return;
    }

    let fresh = table_extract::generate(&libvpx);
    let committed = include_str!("generated.rs");
    if fresh != committed {
        // Find the first differing line for a readable failure.
        for (i, (a, b)) in fresh.lines().zip(committed.lines()).enumerate() {
            if a != b {
                panic!(
                    "generated.rs is stale at line {}:\n  fresh:     {}\n  committed: {}\n\
                     Re-run: cargo run -p videocall-codecs --example extract_vp9_tables",
                    i + 1,
                    a,
                    b
                );
            }
        }
        panic!(
            "generated.rs differs in length (fresh {} vs committed {} bytes)",
            fresh.len(),
            committed.len()
        );
    }
}
