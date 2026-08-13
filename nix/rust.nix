# Rust toolchains (rust-overlay) and the cross rustPlatform used to build the
# Linux service binaries from any host.
#
# The trick that keeps macOS builds fast: rust-overlay ships *prebuilt* std for
# *-unknown-linux-musl that runs on Darwin, so no cross rustc is ever
# bootstrapped — only the C cross toolchain (for -sys crates) is built locally.
{ p }:
let
  inherit (p) pkgs pkgsLinux muslTarget;

  stableVersion = "1.93.1";

  # Native toolchains for dev shells.
  frontendRustMinimal = pkgs.rust-bin.stable.${stableVersion}.minimal.override {
    targets = [ "wasm32-unknown-unknown" ];
  };
  frontendRustDev = pkgs.rust-bin.stable.${stableVersion}.default.override {
    targets = [ "wasm32-unknown-unknown" ];
    extensions = [ "rust-src" "rust-analyzer" ];
  };
  backendRustMinimal = pkgs.rust-bin.stable.${stableVersion}.minimal;
  backendRustDev = pkgs.rust-bin.stable.${stableVersion}.default.override {
    extensions = [ "rust-src" "rust-analyzer" ];
  };
  # Leptos 0.8 targets stable Rust, so the leptos-website toolchains ride the
  # same stable channel as everything else (no nightly needed anymore).
  leptosRustMinimal = pkgs.rust-bin.stable.${stableVersion}.minimal.override {
    targets = [ "wasm32-unknown-unknown" ];
  };
  leptosRustDev = pkgs.rust-bin.stable.${stableVersion}.default.override {
    targets = [ "wasm32-unknown-unknown" ];
    extensions = [ "rust-src" "rust-analyzer" ];
  };

  # cargo-leptos build toolchain: stable, able to target both the browser (wasm)
  # and the image payload (musl).
  leptosRustBuild = pkgs.rust-bin.stable.${stableVersion}.minimal.override {
    targets = [ "wasm32-unknown-unknown" muslTarget ];
  };

  # Host-run toolchain that can *target* Linux musl — feeds rustPlatformCross.
  rustCross = pkgs.rust-bin.stable.${stableVersion}.minimal.override {
    targets = [ muslTarget ];
  };

  # rustPlatform whose stdenv is the musl cross set, but whose rustc/cargo are
  # the native host toolchain above. buildRustPackage wires CARGO_BUILD_TARGET,
  # the cross linker and cc automatically.
  #
  # Deliberately the *dynamic* musl set, not pkgsLinuxStatic: Rust musl targets
  # default to +crt-static so the binaries are static regardless, while the
  # isStatic stdenv leaks `-static` into build-script links for the *build*
  # platform — which explodes on darwin (ld: library not found for -lcrt0.o).
  rustPlatformCross = pkgsLinux.makeRustPlatform {
    cargo = rustCross;
    rustc = rustCross;
  };
in
{
  inherit
    stableVersion
    frontendRustMinimal
    frontendRustDev
    backendRustMinimal
    backendRustDev
    leptosRustMinimal
    leptosRustDev
    leptosRustBuild
    rustCross
    rustPlatformCross;
}
