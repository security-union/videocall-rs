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

//! Compiles and links the `VideocallCapture` Swift static library.
//!
//! On Apple targets (`macos`, `ios`) this derives the Swift target triple and
//! SDK from the Cargo target, invokes `swift build -c release`, and emits the
//! link flags needed to statically link the resulting archive (plus the
//! AVFoundation/CoreMedia/CoreVideo frameworks and the Swift runtime) into the
//! consuming binary. On every other target it is a no-op, so Linux/Windows CI
//! is unaffected.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" && target_os != "ios" {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let swift_dir = manifest_dir.join("swift");

    // Rebuild whenever the Swift sources or package manifest change.
    println!(
        "cargo:rerun-if-changed={}",
        swift_dir.join("Sources").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        swift_dir.join("Package.swift").display()
    );

    let plan = BuildPlan::from_env(&target_os);

    // Isolate the SwiftPM build under OUT_DIR so different Cargo targets do not
    // clobber one another's artifacts (and the source tree stays clean).
    let scratch = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("swift-build");

    let common_args = plan.swift_build_args(&scratch);

    // 1. Build the static library.
    let status = Command::new("swift")
        .current_dir(&swift_dir)
        .args(&common_args)
        .status()
        .expect("failed to spawn `swift build` — is the Swift toolchain installed?");
    assert!(status.success(), "`swift build` failed for {}", plan.triple);

    // 2. Ask SwiftPM where it put the products (robust across toolchain layouts).
    let bin_path = {
        let mut args = common_args.clone();
        args.push("--show-bin-path".to_string());
        let output = Command::new("swift")
            .current_dir(&swift_dir)
            .args(&args)
            .output()
            .expect("failed to query `swift build --show-bin-path`");
        assert!(
            output.status.success(),
            "`swift build --show-bin-path` failed"
        );
        String::from_utf8(output.stdout)
            .expect("non-UTF8 bin path")
            .trim()
            .to_string()
    };

    // 3. Link the static library.
    println!("cargo:rustc-link-search=native={bin_path}");
    println!("cargo:rustc-link-lib=static=VideocallCapture");

    // 4. Link the Apple frameworks the Swift code depends on.
    for framework in ["AVFoundation", "CoreMedia", "CoreVideo", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    // 5. Point the linker at the Swift runtime. Swift object files carry
    //    autolink hints (`-lswiftCore`, …); giving the linker the search paths
    //    lets it resolve them. Both the toolchain copy and the OS copy are
    //    listed so device/simulator/macOS all resolve.
    if let Some(toolchain_swift_lib) = toolchain_swift_lib_dir(&plan.swift_platform) {
        println!(
            "cargo:rustc-link-search=native={}",
            toolchain_swift_lib.display()
        );
    }
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
}

/// The per-target facts needed to build and link the Swift package.
struct BuildPlan {
    /// Swift target triple, e.g. `arm64-apple-ios`.
    triple: String,
    /// `xcrun --sdk` name, e.g. `iphoneos`.
    sdk: String,
    /// Swift runtime platform subdirectory, e.g. `iphoneos`.
    swift_platform: String,
}

impl BuildPlan {
    fn from_env(target_os: &str) -> Self {
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let swift_arch = if arch == "aarch64" {
            "arm64".to_string()
        } else {
            arch
        };
        // The full Cargo triple distinguishes device / simulator / catalyst,
        // which `CARGO_CFG_TARGET_OS` alone does not.
        let target = std::env::var("TARGET").unwrap_or_default();

        if target_os == "macos" {
            return BuildPlan {
                triple: format!("{swift_arch}-apple-macosx"),
                sdk: "macosx".to_string(),
                swift_platform: "macosx".to_string(),
            };
        }

        // target_os == "ios": device, simulator, or Mac Catalyst. Order matters:
        // Catalyst is checked first (its triple also contains x86_64/arm64), then
        // the simulator. Crucially, `x86_64-apple-ios` is the *Intel simulator*
        // triple even though it has no `-sim` suffix — there are no Intel iOS
        // devices — so an x86_64 iOS target is always the simulator.
        if target.ends_with("macabi") {
            // Mac Catalyst builds against the macOS SDK with the macabi ABI.
            BuildPlan {
                triple: format!("{swift_arch}-apple-ios-macabi"),
                sdk: "macosx".to_string(),
                swift_platform: "maccatalyst".to_string(),
            }
        } else if target.ends_with("-sim") || target.contains("ios-sim") || swift_arch == "x86_64" {
            BuildPlan {
                triple: format!("{swift_arch}-apple-ios-simulator"),
                sdk: "iphonesimulator".to_string(),
                swift_platform: "iphonesimulator".to_string(),
            }
        } else {
            BuildPlan {
                triple: format!("{swift_arch}-apple-ios"),
                sdk: "iphoneos".to_string(),
                swift_platform: "iphoneos".to_string(),
            }
        }
    }

    /// The `swift build` argument vector (shared by the build and the
    /// `--show-bin-path` query so they resolve to the same products).
    fn swift_build_args(&self, scratch: &Path) -> Vec<String> {
        let sdk_path = sdk_path(&self.sdk);
        vec![
            "build".to_string(),
            "-c".to_string(),
            "release".to_string(),
            "--triple".to_string(),
            self.triple.clone(),
            "--scratch-path".to_string(),
            scratch.display().to_string(),
            // Pin the SDK explicitly so cross-compiles (iOS from a macOS host)
            // find the right headers and frameworks.
            "-Xswiftc".to_string(),
            "-sdk".to_string(),
            "-Xswiftc".to_string(),
            sdk_path.clone(),
            "-Xcc".to_string(),
            "-isysroot".to_string(),
            "-Xcc".to_string(),
            sdk_path,
        ]
    }
}

/// Resolve an SDK path via `xcrun --sdk <name> --show-sdk-path`.
fn sdk_path(sdk: &str) -> String {
    let output = Command::new("xcrun")
        .args(["--sdk", sdk, "--show-sdk-path"])
        .output()
        .unwrap_or_else(|_| panic!("failed to run `xcrun --sdk {sdk} --show-sdk-path`"));
    assert!(
        output.status.success(),
        "xcrun could not locate the {sdk} SDK"
    );
    String::from_utf8(output.stdout)
        .expect("non-UTF8 SDK path")
        .trim()
        .to_string()
}

/// The toolchain's Swift runtime directory for `platform`
/// (`.../usr/lib/swift/<platform>`), derived from `xcrun --find swiftc`.
fn toolchain_swift_lib_dir(platform: &str) -> Option<PathBuf> {
    let output = Command::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let swiftc = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    // swiftc lives at <toolchain>/usr/bin/swiftc → <toolchain>/usr/lib/swift/<platform>
    let usr = swiftc.parent()?.parent()?;
    Some(usr.join("lib").join("swift").join(platform))
}
