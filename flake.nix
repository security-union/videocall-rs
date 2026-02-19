{
  description = "videocall-rs - WebTransport video calling platform";

  inputs = {
    nixpkgs.url =
      "github:NixOS/nixpkgs/d1c15b7d5806069da59e819999d70e1cec0760bf";

    # cargo-leptos 0.2.42 lives in this older nixpkgs (0.2.x is required for
    # leptos 0.5.x; the main nixpkgs has jumped to 0.3.x).
    nixpkgs-leptos.url =
      "github:NixOS/nixpkgs/ee09932cedcef15aaf476f9343d1dea2cb77e261";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, nixpkgs-leptos, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        pkgsLeptos = import nixpkgs-leptos { inherit system; };

        # leptos-website: pinned nightly required by cargo-leptos 0.2.x
        leptosRustMinimal = pkgs.rust-bin.nightly."2024-11-01".minimal.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        leptosRustDev = pkgs.rust-bin.nightly."2024-11-01".default.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        # yew-ui: pinned stable (needs >= 1.85 for edition 2024 deps)
        yewRustMinimal = pkgs.rust-bin.stable."1.93.1".minimal.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        yewRustDev = pkgs.rust-bin.stable."1.93.1".default.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [ "rust-src" "rust-analyzer" ];
        };

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

        yewBuildInputs = [
          pkgs.trunk
          pkgs.wasm-bindgen-cli_0_2_108
          pkgs.tailwindcss
        ] ++ coreInputs;

        leptosEnv = {
          LEPTOS_HASH_FILES = "false";
          LEPTOS_TAILWIND_VERSION = "v3.4.17";
        };

        # trunk 0.21.x reads NO_COLOR but chokes on the value "1" that
        # mkShell injects; fully unsetting it avoids the clash.
        yewHook = ''
          unset NO_COLOR
        '';

        # Backend: native Rust servers (actix-api, meeting-api, bot)
        backendRustMinimal = pkgs.rust-bin.stable."1.93.1".minimal;

        backendRustDev = pkgs.rust-bin.stable."1.93.1".default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        backendBuildInputs = [
          pkgs.pkg-config
          pkgs.openssl
          pkgs.git
          pkgs.dbmate
          pkgs.cmake
          pkgs.nasm
        ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
          pkgs.libvpx
          pkgs.alsa-lib
          pkgs.libclang
        ];

        backendEnv = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };
      in
      {
        devShells.leptos-website = pkgs.mkShell (leptosEnv // {
          nativeBuildInputs = [ leptosRustMinimal ] ++ leptosBuildInputs;
        });

        devShells.leptos-website-dev = pkgs.mkShell (leptosEnv // {
          nativeBuildInputs = [ leptosRustDev ] ++ leptosBuildInputs;
        });

        devShells.yew-ui = pkgs.mkShell {
          nativeBuildInputs = [ yewRustMinimal ] ++ yewBuildInputs;
          shellHook = yewHook;
        };

        devShells.yew-ui-dev = pkgs.mkShell {
          nativeBuildInputs = [ yewRustDev ] ++ yewBuildInputs;
          shellHook = yewHook;
        };

        devShells.backend = pkgs.mkShell (backendEnv // {
          nativeBuildInputs = [ backendRustMinimal ] ++ backendBuildInputs;
        });

        devShells.backend-dev = pkgs.mkShell (backendEnv // {
          nativeBuildInputs = [ backendRustDev pkgs.cargo-watch ] ++ backendBuildInputs;
        });

        devShells.default = self.devShells.${system}.yew-ui-dev;
      }
    );
}
