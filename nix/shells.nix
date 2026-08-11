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

  # Pinned browser stack for the dioxus-ui wasm component tests. Chrome and
  # chromedriver come from the same nixpkgs pin so browser and driver move in
  # lockstep, and only when nix/tamal pins are bumped. Guarded by availableOn
  # because google-chrome has no aarch64-linux build.
  chromePinned = lib.meta.availableOn pkgs.stdenv.hostPlatform pkgs.google-chrome
    && lib.meta.availableOn pkgs.stdenv.hostPlatform pkgs.chromedriver;
  # chromedriver's browser discovery looks for a binary named "google-chrome";
  # the nix package only ships "google-chrome-stable", so alias it.
  googleChromeAlias = pkgs.runCommand "google-chrome-alias" { } ''
    mkdir -p $out/bin
    ln -s ${pkgs.google-chrome}/bin/google-chrome-stable $out/bin/google-chrome
  '';
  # Single entry point for the dioxus-ui wasm component tests:
  #   nix-shell -A shells.frontend-tests --run dioxus-ui-component-tests
  # writeShellApplication shellchecks the script at build time. The pinned
  # browser is Linux-only, so on darwin the script ships without it and is
  # run with DIOXUS_UI_TESTS_SYSTEM_BROWSER=1 against the system Chrome
  # (see the script header).
  dioxusUiComponentTests = pkgs.writeShellApplication {
    name = "dioxus-ui-component-tests";
    runtimeInputs = [ pkgs.jq ] ++ lib.optionals chromePinned [
      pkgs.google-chrome
      googleChromeAlias
      pkgs.chromedriver
    ];
    text = builtins.readFile ./dioxus-ui-component-tests.sh;
  };

  browserTestInputs = [ dioxusUiComponentTests ] ++ lib.optionals chromePinned [
    pkgs.google-chrome
    googleChromeAlias
    pkgs.chromedriver
  ];
  browserTestEnv = lib.optionalAttrs chromePinned {
    CHROMEDRIVER = "${pkgs.chromedriver}/bin/chromedriver";
  };

  frontendBuildInputs = [
    pkgs.trunk
    pkgs.wasm-bindgen-cli_0_2_108
    pkgs.tailwindcss
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

  # frontend + the pinned browser stack; only needed to run the dioxus-ui wasm
  # component tests, so it's a separate shell to keep plain `nix-shell` from
  # downloading google-chrome.
  frontend-tests = pkgs.mkShell (browserTestEnv // {
    nativeBuildInputs = [ rust.frontendRustMinimal ] ++ frontendBuildInputs
      ++ browserTestInputs;
    shellHook = frontendHook;
  });

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
