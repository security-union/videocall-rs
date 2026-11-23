# Dev shells. Optional for day-to-day dev (rustup works too) — they provide
# the *pinned* toolchains used by CI.
#
#   nix-shell                                            -> default (frontend-dev)
#   nix-shell default.nix -A shells.backend-dev --run …  -> named shell
#   nix-shell default.nix -A shells.dev --run dev-stack  -> whole native stack (make dev)
{ p, rust, devStack, e2eStack, packages }:
let
  inherit (p) pkgs pkgsLeptos;
  lib = pkgs.lib;

  coreInputs = [
    pkgs.binaryen
    pkgs.pkg-config
    pkgs.openssl
    pkgs.git
  ];

  leptosBuildInputs = [
    pkgsLeptos.cargo-leptos
    pkgs.wasm-bindgen-cli_0_2_100
    pkgs.nodejs_20
  ] ++ coreInputs;

  # Single entry point for the dioxus-ui wasm component tests:
  #   nix-shell -A shells.frontend-tests --run dioxus-ui-component-tests
  # writeShellApplication shellchecks the script at build time. The browser
  # is deliberately NOT nix-pinned: Google's apt pool only hosts the current
  # stable google-chrome .deb, so a nixpkgs pin of it stops fetching as soon
  # as Chrome ships a new release. CI installs current stable Chrome plus the
  # matching chromedriver with browser-actions/setup-chrome and passes them
  # in via CHROME_BIN/CHROMEDRIVER (see the script header).
  dioxusUiComponentTests = pkgs.writeShellApplication {
    name = "dioxus-ui-component-tests";
    runtimeInputs = [ pkgs.jq pkgs.coreutils ];
    text = builtins.readFile ./dioxus-ui-component-tests.sh;
  };

  frontendBuildInputs = [
    pkgs.trunk
    # Must match the wasm-bindgen crate in Cargo.lock; see packages.nix.
    packages.wasmBindgenCli
  ] ++ coreInputs;

  leptosEnv = {
    LEPTOS_HASH_FILES = "false";
    LEPTOS_TAILWIND_VERSION = "v3.4.17";
  };

  # trunk 0.21.x reads NO_COLOR but chokes on the value "1" that mkShell
  # injects; fully unsetting it avoids the clash.
  frontendHook = ''
    unset NO_COLOR
  '';

  backendBuildInputs = [
    pkgs.pkg-config
    pkgs.openssl
    pkgs.git
    pkgs.dbmate
    pkgs.cmake
    pkgs.nasm
  ] ++ lib.optionals pkgs.stdenv.isLinux [
    pkgs.libvpx
    pkgs.alsa-lib
    pkgs.libclang
  ];

  backendEnv = lib.optionalAttrs pkgs.stdenv.isLinux {
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
  };

  frontendDev = pkgs.mkShell {
    nativeBuildInputs = [ rust.frontendRustDev ] ++ frontendBuildInputs;
    shellHook = frontendHook;
  };
in
{
  leptos-website = pkgs.mkShell (leptosEnv // {
    nativeBuildInputs = [ rust.leptosRustMinimal ] ++ leptosBuildInputs;
  });

  leptos-website-dev = pkgs.mkShell (leptosEnv // {
    nativeBuildInputs = [ rust.leptosRustDev ] ++ leptosBuildInputs;
  });

  frontend = pkgs.mkShell {
    nativeBuildInputs = [ rust.frontendRustMinimal ] ++ frontendBuildInputs;
    shellHook = frontendHook;
  };

  # frontend + the dioxus-ui wasm component test runner. The browser and
  # chromedriver come from outside nix (see dioxusUiComponentTests above).
  frontend-tests = pkgs.mkShell {
    nativeBuildInputs = [ rust.frontendRustMinimal ] ++ frontendBuildInputs
      ++ [ dioxusUiComponentTests ];
    shellHook = frontendHook;
  };

  frontend-dev = frontendDev;

  backend = pkgs.mkShell (backendEnv // {
    nativeBuildInputs = [ rust.backendRustMinimal ] ++ backendBuildInputs;
  });

  # CI shell for the wasm test matrix: wasm-pack/node for videocall-client,
  # plus the libvpx/bindgen stack the videocall-codecs oracle tests need.
  wasm-tests = pkgs.mkShell (backendEnv // {
    nativeBuildInputs = [
      rust.frontendRustMinimal
      pkgs.wasm-pack
      # wasm-pack uses a wasm-bindgen on PATH when its version matches
      # Cargo.lock, instead of downloading one.
      packages.wasmBindgenCli
      pkgs.nodejs_20
      pkgs.cmake
      pkgs.nasm
    ] ++ coreInputs ++ lib.optionals pkgs.stdenv.isLinux [
      pkgs.libvpx
      pkgs.libclang
    ];
  });

  backend-dev = pkgs.mkShell (backendEnv // {
    nativeBuildInputs = [ rust.backendRustDev pkgs.cargo-watch pkgs.cargo-machete pkgs.nixtamal ]
      ++ backendBuildInputs;
  });

  # `make dev`: the full native stack (process-compose). Backend + frontend
  # toolchains plus the dev-stack supervisor script.
  dev = pkgs.mkShell (backendEnv // {
    nativeBuildInputs = [
      # frontendRustMinimal = stable 1.93.1 with the native host target AND
      # wasm32-unknown-unknown, so one toolchain serves cargo-watch'd servers
      # and the trunk-built UI alike.
      rust.frontendRustMinimal
      pkgs.cargo-watch
      pkgs.process-compose # attach/list/down against a running dev-stack
      devStack
    ] ++ backendBuildInputs ++ frontendBuildInputs;
    shellHook = frontendHook;
  });

  # Protobuf codegen: `protoc --rs_out=…` with the pinned protoc-gen-rs
  # plugin (replaces the old docker build-env container; see protobuf/Makefile)
  protobuf = pkgs.mkShell {
    nativeBuildInputs = [ pkgs.protobuf packages.protobufCodegen ];
  };

  # NetEQ parity harness (neteq/parity): compiles Google WebRTC's C++ NetEQ
  # and diffs our Rust port against it. C++20 from the pinned stdenv (clang on
  # darwin, gcc on linux), abseil + libopus from the same pin so they share
  # one C++ runtime. libopus is the fixed-point build (what Chromium ships on
  # ARM): the Rust side decodes with ropus, a port of fixed-point libopus, so
  # only this build can match it sample for sample. Intrinsics are off for
  # the same reason the WebRTC SPL is built without NEON (generic C kernels
  # only), and because 1.5.2's meson build fails to link fixed-point NEON on
  # aarch64.
  #   nix-shell default.nix -A shells.neteq-parity --run 'cd neteq/parity && cargo test'
  neteq-parity = let
    # Google WebRTC at the rev the harness is validated against, sparse
    # (same dirs as neteq/parity/scripts/fetch_webrtc.sh, ~17 MB). A
    # fixed-output fetch has no .git, so the rev travels as WEBRTC_REV and
    # build.rs checks that instead of `git rev-parse`. The rev is in the
    # store name on purpose: Nix finds a fixed-output path by name and hash
    # alone, so bumping only `webrtcRev` would silently reuse the old
    # sources. With the rev in the name, a bump refetches and a stale hash
    # fails loudly.
    webrtcRev = "6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd";
    webrtcSrc = pkgs.fetchgit {
      name = "webrtc-neteq-src-${builtins.substring 0 12 webrtcRev}";
      url = "https://webrtc.googlesource.com/src";
      rev = webrtcRev;
      sparseCheckout = [
        "api"
        "common_audio"
        "modules/audio_coding/codecs"
        "modules/audio_coding/neteq"
        "modules/include"
        "modules/rtp_rtcp/include"
        "modules/rtp_rtcp/source"
        "rtc_base"
        "system_wrappers"
      ];
      hash = "sha256-sOjgyUpj8on6BQMfE1uaqMhiKwpRmbNwrkRL++t+RQw=";
    };
    libopusFixedC = (pkgs.libopus.override { fixedPoint = true; }).overrideAttrs (old: rec {
      version = "1.6.1";
      src = pkgs.fetchurl {
        url = "https://downloads.xiph.org/releases/opus/opus-${version}.tar.gz";
        sha256 = "188i4ibr80az4217q04z22cyrfv4xmk29cqmvy22bsbv429vbz3g";
      };
      # The pin's patch targets 1.5.2's test meson file; tests are off anyway.
      patches = [ ];
      mesonFlags = map
        (flag:
          if flag == "-Dintrinsics=enabled" then "-Dintrinsics=disabled"
          else if flag == "-Drtcd=enabled" then "-Drtcd=disabled"
          else flag)
        old.mesonFlags;
      doCheck = false;
    });
  in pkgs.mkShell {
    nativeBuildInputs = [
      rust.backendRustDev
      pkgs.pkg-config
      pkgs.git
    ];
    buildInputs = [
      pkgs.abseil-cpp
      libopusFixedC
    ];
    # The cc crate passes --target arm64-apple-macosx; the nix clang wrapper
    # warns on every file about the spelling. It is the same target.
    NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";
    WEBRTC_SRC = webrtcSrc;
    WEBRTC_REV = webrtcRev;
  };

  # `make vlog`: serve the engineering vlog with the same zola the image uses
  vlog = pkgs.mkShell {
    nativeBuildInputs = [ pkgs.zola ];
  };

  # Playwright e2e stack, no Docker. Linux runs the nix-built server binaries
  # so this shell stays slim; darwin builds them with cargo and needs the
  # backend toolchain.
  e2e = pkgs.mkShell (backendEnv // {
    nativeBuildInputs = [ e2eStack pkgs.process-compose ]
      ++ lib.optionals (!pkgs.stdenv.isLinux)
        ([ rust.backendRustMinimal ] ++ backendBuildInputs);
  });

  default = frontendDev;
}
