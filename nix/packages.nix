# Service binaries and the UI dist as Nix derivations, built with crane so
# dependency compilation is a SEPARATE, cached derivation:
#
#   nativeDeps  — every workspace dependency compiled for musl Linux, once.
#                 Shared by videocall-api-bins, meeting-api and bot; cached by
#                 the CI nix cache and only rebuilt when Cargo.lock/toml move.
#   wasmDeps    — dependencies of the three wasm crates (ui, codecs, neteq),
#                 shared by the trunk build.
#
# Without this split every buildRustPackage recompiled the full dependency
# graph per package (the role the old `actix-base` docker image played).
{ p
, rust
, inputs
  # Build metadata baked into /version endpoints. The build.rs files prefer
  # these env vars over shelling out to git (which is absent in the sandbox).
  # Fixed defaults keep derivations reproducible; CI overrides via release.nix.
, gitSha ? "unknown"
, gitBranch ? "nix"
, buildTimestamp ? "unknown"
}:
let
  inherit (p) pkgs pkgsLinux pkgsLinuxStatic muslTarget;
  lib = pkgs.lib;

  # crane, flake-free: default.nix is { pkgs ? … }: callPackage ./lib { }
  mkCraneLib = pkgsForLib: import inputs.crane { pkgs = pkgsForLib; };

  # Cross/native musl builds: crane over the Linux package set (so cc/stdenv
  # are the musl cross toolchain) driving the host rustc that targets musl.
  craneLibNative = (mkCraneLib pkgsLinux).overrideToolchain rust.rustCross;

  # wasm builds run entirely on the host (arch-neutral output).
  craneLibWasm = (mkCraneLib pkgs).overrideToolchain rust.frontendRustMinimal;

  # Workspace source, filtered to what cargo actually needs. Cargo resolves
  # every workspace member's manifest (and their path deps), so crate dirs
  # stay; only build outputs, VCS state and non-Rust trees are dropped.
  src = lib.cleanSourceWith {
    name = "videocall-rs-src";
    src = ../.;
    filter = path: type:
      let
        rel = lib.removePrefix (toString ../. + "/") (toString path);
        topLevel = builtins.elemAt (lib.splitString "/" rel) 0;
        base = baseNameOf path;
      in
      # dirs excluded anywhere in the tree
      !(builtins.elem base [ "target" "dist" "node_modules" ".git" ])
      # non-Rust / non-build trees excluded at the top level
      && !(builtins.elem topLevel [
        ".github"
        ".vscode"
        ".cursor"
        ".claude"
        "apps"
        "devices"
        "docker"
        "docs"
        "e2e"
        "engineering-vlog"
        "helm"
        "helm-videocall-deployment"
        "leptos-website"
        "src-tauri"
        "scripts"
      ]);
  };

  buildMetaEnv = {
    GIT_SHA = gitSha;
    GIT_BRANCH = gitBranch;
    BUILD_TIMESTAMP = buildTimestamp;
  };

  # ---- native (musl) pipeline ----------------------------------------------

  # NOTE 1: buildMetaEnv is deliberately NOT part of these args — it changes
  # per commit in the publish workflows (--argstr gitSha …) and would churn the
  # deps-only derivation, defeating the dependency cache. It is applied only
  # to the (cheap) member builds below.
  # NOTE 2: no hand-rolled CARGO_BUILD_TARGET / CARGO_TARGET_*_LINKER here —
  # craneLibNative sits on the musl cross set, so crane's cross-toolchain env
  # supplies both (with an absolute store-path linker, immune to PATH games).
  nativeCommonArgs = {
    inherit src;
    version = "0.1.0";
    strictDeps = true;
    doCheck = false; # integration tests need postgres/nats; `make tests_run`

    nativeBuildInputs = [
      pkgs.cmake # aws-lc-sys
      pkgs.nasm
      pkgs.perl # aws-lc-sys / openssl-sys
      pkgs.pkg-config
    ];
    # Static openssl on purpose (not pkgsLinux.openssl): openssl-sys links the
    # .a and the images ship no libssl.so. Same-arch pkg-config makes the
    # cross-set reach safe here.
    buildInputs = [ pkgsLinuxStatic.openssl ];
  };

  # Dependencies of the three members we ship, compiled once for musl. THE
  # cache unit. Scoped with -p on purpose: the full workspace would drag in
  # videocall-cli's native capture stack (alsa/libvpx via cpal/nokhwa), which
  # is never shipped in an image and doesn't cross-compile to musl.
  nativeDeps = craneLibNative.buildDepsOnly (nativeCommonArgs // {
    pname = "videocall-native-deps";
    cargoExtraArgs = "-p videocall-api -p meeting-api -p bot";
  });

  # One workspace package's binaries on top of the shared dep artifacts.
  mkNativePackage = { pname, cargoExtraArgs, mainProgram }:
    craneLibNative.buildPackage (nativeCommonArgs // buildMetaEnv // {
      inherit pname cargoExtraArgs;
      cargoArtifacts = nativeDeps;
      # only this member compiles here; deps come from nativeDeps
      meta = { inherit mainProgram; };
    });

  # ---- wasm pipeline --------------------------------------------------------

  wasmCommonArgs = {
    inherit src;
    version = "0.1.0";
    doCheck = false;
    CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
  };

  # Trunk runs THREE cargo invocations (index.html: the UI crate + two worker
  # bins), each resolving features per-invocation. A single union deps build
  # produces artifacts whose fingerprints match none of them — cargo silently
  # recompiles web-sys and friends inside the dist build. So: chain one
  # deps-only derivation per invocation, each with that invocation's exact
  # feature set, accumulating into one artifacts dir the dist build inherits.
  wasmDepsUi = craneLibWasm.buildDepsOnly (wasmCommonArgs // {
    pname = "wasm-deps-ui";
    cargoExtraArgs = "-p videocall-ui";
  });
  wasmDepsCodecs = craneLibWasm.buildDepsOnly (wasmCommonArgs // {
    pname = "wasm-deps-codecs";
    cargoArtifacts = wasmDepsUi;
    cargoExtraArgs = "-p videocall-codecs --no-default-features --features wasm";
  });
  wasmDeps = craneLibWasm.buildDepsOnly (wasmCommonArgs // {
    pname = "wasm-deps";
    cargoArtifacts = wasmDepsCodecs;
    cargoExtraArgs = "-p neteq --no-default-features --features web";
  });
in
{
  # The cache units, exposed so CI can warm them serially before fanning out
  # (and so `nix-build release.nix -A packages.nativeDeps` works by hand).
  inherit nativeDeps wasmDeps;

  # EVERYTHING the image legs need except the per-commit member builds: the
  # crane deps artifacts plus the image runtime payloads. The audit of leg
  # job 93372834164 showed the legs otherwise rebuild go-1.25.5 (for static
  # dbmate), busybox, caddy, musl tzdata/cacert on EVERY run — and never save
  # them, because they exact-hit the warm cache key and skip their own save.
  ciWarm = pkgs.linkFarm "ci-warm" [
    { name = "native-deps"; path = nativeDeps; }
    { name = "wasm-deps"; path = wasmDeps; }
    { name = "caddy"; path = p.caddyStatic; }
    { name = "busybox"; path = pkgsLinuxStatic.busybox; }
    { name = "dbmate"; path = pkgsLinuxStatic.dbmate; }
    { name = "tzdata"; path = pkgsLinux.tzdata; }
    { name = "cacert"; path = pkgsLinux.dockerTools.caCertificates; }
  ];

  # All four actix-api server binaries in one compile — mirrors the old
  # Dockerfile.actix, and lets the per-service images share one derivation:
  # websocket_server, webtransport_server, metrics_server, metrics_server_snapshot.
  videocall-api-bins = mkNativePackage {
    pname = "videocall-api-bins";
    cargoExtraArgs = "-p videocall-api";
    mainProgram = "websocket_server";
  };

  meeting-api = mkNativePackage {
    pname = "meeting-api";
    cargoExtraArgs = "-p meeting-api --bin meeting-api";
    mainProgram = "meeting-api";
  };

  bot = mkNativePackage {
    pname = "bot";
    cargoExtraArgs = "-p bot";
    mainProgram = "bot";
  };

  # Dioxus UI static site: trunk build of the main crate + the two wasm worker
  # bins wired in index.html (videocall-codecs worker_decoder, neteq
  # neteq_worker), tailwind compile, and version.json — replicating the old
  # Dockerfile.dioxus builder stage on top of the shared wasm dep artifacts.
  dioxus-ui-dist = craneLibWasm.mkCargoDerivation (buildMetaEnv // wasmCommonArgs // {
    pname = "dioxus-ui-dist";
    cargoArtifacts = wasmDeps;

    nativeBuildInputs = [
      pkgs.trunk
      pkgs.wasm-bindgen-cli_0_2_108
      pkgs.binaryen
      pkgs.tailwindcss
    ];

    buildPhaseCargoCommand = ''
      # trunk 0.21.x reads NO_COLOR but chokes on the value "1"; it also
      # wants a writable HOME for its cache.
      unset NO_COLOR
      export HOME="$TMPDIR"
      pushd dioxus-ui
      tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --minify
      trunk build --release --offline --skip-version-check --locked
      version=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
      printf '{"service":"dioxus-ui","version":"%s","git_sha":"%s","git_branch":"%s","build_timestamp":"%s"}\n' \
        "$version" "$(echo "$GIT_SHA" | cut -c1-8)" "$GIT_BRANCH" "$BUILD_TIMESTAMP" \
        > dist/version.json
      popd
    '';

    installPhaseCommand = ''
      mkdir -p $out
      cp -r dioxus-ui/dist/. $out/
    '';
  });
}
