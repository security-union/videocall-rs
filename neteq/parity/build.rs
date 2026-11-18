//! Compiles Google WebRTC's NetEq (plus the minimum of api/, rtc_base/,
//! common_audio/ and system_wrappers/ it links against) and our C ABI shim
//! into static libraries.
//!
//! Inputs:
//! - `WEBRTC_SRC`: path to a WebRTC checkout at [`PINNED_REV`] (see
//!   `scripts/fetch_webrtc.sh`). Falls back to `<crate>/third_party/webrtc`.
//!   The nix shell points it at a pinned `fetchgit` store path.
//! - `WEBRTC_REV`: the revision of `WEBRTC_SRC` when it has no `.git` (the
//!   nix shell sets it). Otherwise the rev comes from `git rev-parse HEAD`.
//! - `abseil-cpp` and `opus` through pkg-config (the nix shell provides both).
//!
//! No WebRTC file is patched. Two GN-generated / third_party paths are
//! satisfied with forwarding headers written to `OUT_DIR/inc`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// WebRTC revision the harness is validated against.
const PINNED_REV: &str = "6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd";

/// C++ sources, relative to `WEBRTC_SRC`. The NetEq list mirrors
/// `rtc_library("neteq")` in modules/audio_coding/BUILD.gn; the rest is the
/// transitive closure the linker actually needs.
const CXX_SOURCES: &[&str] = &[
    // modules/audio_coding/neteq (rtc_library("neteq"))
    "modules/audio_coding/neteq/accelerate.cc",
    "modules/audio_coding/neteq/audio_multi_vector.cc",
    "modules/audio_coding/neteq/audio_vector.cc",
    "modules/audio_coding/neteq/background_noise.cc",
    "modules/audio_coding/neteq/buffer_level_filter.cc",
    "modules/audio_coding/neteq/comfort_noise.cc",
    "modules/audio_coding/neteq/cross_correlation.cc",
    "modules/audio_coding/neteq/decision_logic.cc",
    "modules/audio_coding/neteq/decoder_database.cc",
    "modules/audio_coding/neteq/delay_constraints.cc",
    "modules/audio_coding/neteq/delay_manager.cc",
    "modules/audio_coding/neteq/dsp_helper.cc",
    "modules/audio_coding/neteq/dtmf_buffer.cc",
    "modules/audio_coding/neteq/dtmf_tone_generator.cc",
    "modules/audio_coding/neteq/expand.cc",
    "modules/audio_coding/neteq/expand_uma_logger.cc",
    "modules/audio_coding/neteq/histogram.cc",
    "modules/audio_coding/neteq/merge.cc",
    "modules/audio_coding/neteq/neteq_impl.cc",
    "modules/audio_coding/neteq/normal.cc",
    "modules/audio_coding/neteq/packet.cc",
    "modules/audio_coding/neteq/packet_arrival_history.cc",
    "modules/audio_coding/neteq/packet_buffer.cc",
    "modules/audio_coding/neteq/preemptive_expand.cc",
    "modules/audio_coding/neteq/random_vector.cc",
    "modules/audio_coding/neteq/red_payload_splitter.cc",
    "modules/audio_coding/neteq/reorder_optimizer.cc",
    "modules/audio_coding/neteq/statistics_calculator.cc",
    "modules/audio_coding/neteq/sync_buffer.cc",
    "modules/audio_coding/neteq/time_stretch.cc",
    "modules/audio_coding/neteq/timestamp_scaler.cc",
    "modules/audio_coding/neteq/underrun_optimizer.cc",
    // Codecs: L16 (PCM16B), Opus (over system libopus), RFC 3389 CNG.
    "modules/audio_coding/codecs/pcm16b/audio_decoder_pcm16b.cc",
    "modules/audio_coding/codecs/pcm16b/pcm16b_common.cc",
    "modules/audio_coding/codecs/opus/audio_decoder_opus.cc",
    "modules/audio_coding/codecs/opus/opus_interface.cc",
    "modules/audio_coding/codecs/opus/audio_coder_opus_common.cc",
    "modules/audio_coding/codecs/cng/webrtc_cng.cc",
    "modules/audio_coding/codecs/legacy_encoded_audio_frame.cc",
    // api/
    "api/neteq/neteq.cc",
    "api/neteq/tick_timer.cc",
    "api/neteq/default_neteq_controller_factory.cc",
    "api/environment/environment_factory.cc",
    "api/environment/deprecated_global_field_trials.cc",
    "api/environment/force_test_environment.cc",
    "api/field_trials_registry.cc",
    "api/audio_codecs/audio_decoder.cc",
    "api/audio_codecs/audio_format.cc",
    "api/audio/audio_frame.cc",
    "api/audio/channel_layout.cc",
    "api/rtp_headers.cc",
    "api/rtp_packet_info.cc",
    "api/rtc_event_log/rtc_event_log.cc",
    "api/units/data_rate.cc",
    "api/units/data_size.cc",
    "api/units/frequency.cc",
    "api/units/time_delta.cc",
    "api/units/timestamp.cc",
    "api/video/color_space.cc",
    "api/video/hdr_metadata.cc",
    // system_wrappers/
    "system_wrappers/source/clock.cc",
    "system_wrappers/source/metrics.cc",
    // rtc_base/
    "rtc_base/checks.cc",
    "rtc_base/ref_counter.cc",
    "rtc_base/strings/string_builder.cc",
    "rtc_base/string_to_number.cc",
    "rtc_base/system_time.cc",
    "rtc_base/time_utils.cc",
    "rtc_base/experiments/field_trial_parser.cc",
    "rtc_base/experiments/field_trial_units.cc",
    "rtc_base/experiments/struct_parameters_parser.cc",
    // common_audio/ (C++ parts of the signal processing library)
    "common_audio/signal_processing/auto_correlation.cc",
    "common_audio/signal_processing/dot_product_with_scale.cc",
];

/// C sources (generic, non-NEON variants: the Rust port mirrors these).
const C_SOURCES: &[&str] = &[
    "modules/audio_coding/codecs/pcm16b/pcm16b.c",
    "common_audio/signal_processing/auto_corr_to_refl_coef.c",
    "common_audio/signal_processing/copy_set_operations.c",
    "common_audio/signal_processing/cross_correlation.c",
    "common_audio/signal_processing/division_operations.c",
    "common_audio/signal_processing/downsample_fast.c",
    "common_audio/signal_processing/energy.c",
    "common_audio/signal_processing/filter_ar.c",
    "common_audio/signal_processing/filter_ar_fast_q12.c",
    "common_audio/signal_processing/filter_ma_fast_q12.c",
    "common_audio/signal_processing/get_hanning_window.c",
    "common_audio/signal_processing/get_scaling_square.c",
    "common_audio/signal_processing/levinson_durbin.c",
    "common_audio/signal_processing/lpc_to_refl_coef.c",
    "common_audio/signal_processing/min_max_operations.c",
    "common_audio/signal_processing/randomization_functions.c",
    "common_audio/signal_processing/refl_coef_to_lpc.c",
    "common_audio/signal_processing/spl_init.c",
    "common_audio/signal_processing/spl_inl.c",
    "common_audio/signal_processing/spl_sqrt.c",
    "common_audio/signal_processing/vector_operations.c",
    "common_audio/signal_processing/vector_scaling_operations.c",
    "common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");

    println!("cargo:rerun-if-env-changed=WEBRTC_SRC");
    println!("cargo:rerun-if-env-changed=WEBRTC_REV");
    println!("cargo:rerun-if-changed=cpp");
    println!("cargo:rerun-if-changed=build.rs");

    let webrtc = env::var_os("WEBRTC_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("third_party/webrtc"));
    if !webrtc
        .join("modules/audio_coding/neteq/neteq_impl.cc")
        .exists()
    {
        panic!(
            "WebRTC sources not found at {}. Run `neteq/parity/scripts/fetch_webrtc.sh <dir>` \
             and set WEBRTC_SRC=<dir> (pinned rev {PINNED_REV}).",
            webrtc.display()
        );
    }
    check_revision(&webrtc);

    let gen_inc = out_dir.join("inc");
    write_forwarding_headers(&gen_inc);

    let opus = pkg_config::Config::new()
        .probe("opus")
        .expect("pkg-config: libopus not found (use nix-shell neteq/parity/shell.nix)");
    let absl = pkg_config::Config::new().probe("absl_strings").expect(
        "pkg-config: abseil-cpp (absl_strings) not found (use nix-shell neteq/parity/shell.nix)",
    );

    let platform_define = match target_os.as_str() {
        "macos" | "ios" => "WEBRTC_MAC",
        "linux" | "android" => "WEBRTC_LINUX",
        other => panic!("neteq-parity: unsupported target OS {other}"),
    };

    let configure = |build: &mut cc::Build| {
        build
            .include(&webrtc)
            .include(&gen_inc)
            .include(manifest_dir.join("cpp"))
            .includes(&opus.include_paths)
            .includes(&absl.include_paths)
            .define("WEBRTC_POSIX", None)
            .define(platform_define, None)
            .define("NDEBUG", None)
            .define("RTC_DISABLE_LOGGING", None)
            .define("RTC_DISABLE_TRACE_EVENTS", None)
            .define("WEBRTC_STRICT_FIELD_TRIALS", "0")
            .define("WEBRTC_APM_DEBUG_DUMP", "0")
            .define("WEBRTC_CODEC_OPUS", None)
            .define("WEBRTC_OPUS_SUPPORT_120MS_PTIME", "1")
            // Rust never fuses a*b+c; keep the reference from doing it either.
            .flag_if_supported("-ffp-contract=off")
            .opt_level(2)
            .warnings(false)
            .flag_if_supported("-Wno-nullability-completeness")
            .flag_if_supported("-Wno-deprecated-declarations");
    };

    let mut cxx = cc::Build::new();
    configure(&mut cxx);
    cxx.cpp(true).std("c++20");
    for src in CXX_SOURCES {
        cxx.file(webrtc.join(src));
    }
    cxx.file(manifest_dir.join("cpp/neteq_shim.cc"));
    cxx.file(manifest_dir.join("cpp/link_stubs.cc"));
    cxx.compile("webrtc_neteq_cxx");

    let mut c = cc::Build::new();
    configure(&mut c);
    for src in C_SOURCES {
        c.file(webrtc.join(src));
    }
    c.compile("webrtc_neteq_c");

    // pkg-config already emitted the link lines for opus and absl_strings.
}

/// Warns (does not fail) when the checkout is not at the pinned revision.
///
/// The revision is `WEBRTC_REV` if set (a nix `fetchgit` output has no
/// `.git`), else `git rev-parse HEAD`. If neither is available it says so.
fn check_revision(webrtc: &Path) {
    let rev = env::var("WEBRTC_REV")
        .ok()
        .map(|rev| rev.trim().to_string())
        .filter(|rev| !rev.is_empty())
        .or_else(|| git_head(webrtc));
    match rev {
        Some(rev) if rev == PINNED_REV => {}
        Some(rev) => println!(
            "cargo:warning=WEBRTC_SRC is at {rev}, harness is pinned to {PINNED_REV}; results may differ"
        ),
        None => println!(
            "cargo:warning=cannot tell which revision WEBRTC_SRC is at (no .git, WEBRTC_REV unset); \
             harness is pinned to {PINNED_REV}"
        ),
    }
}

/// `git rev-parse HEAD` of `dir`, if it is a git checkout.
fn git_head(dir: &Path) -> Option<String> {
    // Without its own .git, rev-parse would report an enclosing repo's HEAD.
    if !dir.join(".git").exists() {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Satisfies include paths that WebRTC resolves through its GN build.
fn write_forwarding_headers(gen_inc: &Path) {
    let opus_dir = gen_inc.join("third_party/opus/src/include");
    fs::create_dir_all(&opus_dir).expect("create opus forwarding dir");
    for header in ["opus", "opus_defines", "opus_multistream", "opus_types"] {
        write_if_changed(
            &opus_dir.join(format!("{header}.h")),
            &format!("// Forwarding header to the system libopus.\n#include <{header}.h>\n"),
        );
    }
    let experiments = gen_inc.join("experiments");
    fs::create_dir_all(&experiments).expect("create experiments dir");
    write_if_changed(
        &experiments.join("registered_field_trials.h"),
        "// Stand-in for the GN-generated header. Only read when\n\
         // WEBRTC_STRICT_FIELD_TRIALS is non-zero; the harness builds with 0.\n",
    );
}

fn write_if_changed(path: &Path, contents: &str) {
    if fs::read_to_string(path).ok().as_deref() != Some(contents) {
        fs::write(path, contents).expect("write forwarding header");
    }
}
