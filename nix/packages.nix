# Service binaries as Nix derivations. Each is a statically-linked musl ELF
# built by rustPlatformCross (native on Linux hosts, cross from macOS).
{ p
, rust
  # Build metadata baked into /version endpoints. The build.rs files prefer
  # these env vars over shelling out to git (which is absent in the sandbox).
  # Fixed defaults keep derivations reproducible; CI overrides via release.nix.
, gitSha ? "unknown"
, gitBranch ? "nix"
, buildTimestamp ? "unknown"
}:
let
  inherit (p) pkgs pkgsLinuxStatic;
  lib = pkgs.lib;

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

  # Build one workspace package's binaries.
  #   crate — cargo package name (-p …)
  #   bins  — bin targets to build; empty = every bin of the crate
  mkServiceBinary = { pname, crate, bins ? [ ], mainProgram ? null }:
    rust.rustPlatformCross.buildRustPackage (buildMetaEnv // {
      inherit pname src;
      version = "0.1.0";

      cargoLock.lockFile = ../Cargo.lock;
      cargoBuildFlags = [ "-p" crate ]
        ++ lib.concatMap (b: [ "--bin" b ]) bins;

      # Integration tests need postgres/nats; covered by `make tests_run`.
      doCheck = false;

      nativeBuildInputs = [
        pkgs.cmake # aws-lc-sys
        pkgs.nasm
        pkgs.perl # aws-lc-sys / openssl-sys
        pkgs.pkg-config
      ];
      buildInputs = [ pkgsLinuxStatic.openssl ];

      meta = lib.optionalAttrs (mainProgram != null) { inherit mainProgram; };
    });
in
rec {
  # All four actix-api server binaries in one compile — mirrors the old
  # Dockerfile.actix, and lets the per-service images share one derivation:
  # websocket_server, webtransport_server, metrics_server, metrics_server_snapshot.
  videocall-api-bins = mkServiceBinary {
    pname = "videocall-api-bins";
    crate = "videocall-api";
    mainProgram = "websocket_server";
  };

  meeting-api = mkServiceBinary {
    pname = "meeting-api";
    crate = "meeting-api";
    bins = [ "meeting-api" ];
    mainProgram = "meeting-api";
  };

  bot = mkServiceBinary {
    pname = "bot";
    crate = "bot";
    mainProgram = "bot";
  };

  # Dioxus UI static site: trunk build of the main crate + the two wasm worker
  # bins wired in index.html (videocall-codecs worker_decoder, neteq
  # neteq_worker), tailwind compile, and version.json — replicating the old
  # Dockerfile.dioxus builder stage. wasm32 is arch-neutral, so this builds
  # with the *host* toolchain on every platform; only nginx in the image layer
  # is Linux-specific.
  dioxus-ui-dist = pkgs.stdenv.mkDerivation (buildMetaEnv // {
    pname = "dioxus-ui-dist";
    version = "0.1.0";
    inherit src;

    cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ../Cargo.lock; };

    nativeBuildInputs = [
      pkgs.rustPlatform.cargoSetupHook
      rust.frontendRustMinimal
      pkgs.trunk
      pkgs.wasm-bindgen-cli_0_2_108
      pkgs.binaryen
      pkgs.tailwindcss
    ];

    # trunk 0.21.x reads NO_COLOR but chokes on the value "1" (same clash the
    # dev shells work around); it also wants a writable HOME for its cache.
    postPatch = ''
      unset NO_COLOR
    '';

    buildPhase = ''
      runHook preBuild
      export HOME="$TMPDIR"
      cd dioxus-ui
      tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --minify
      trunk build --release --offline --skip-version-check --locked
      version=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
      printf '{"service":"dioxus-ui","version":"%s","git_sha":"%s","git_branch":"%s","build_timestamp":"%s"}\n' \
        "$version" "$(echo "$GIT_SHA" | cut -c1-8)" "$GIT_BRANCH" "$BUILD_TIMESTAMP" \
        > dist/version.json
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp -r dist/. $out/
      runHook postInstall
    '';
  });
}
