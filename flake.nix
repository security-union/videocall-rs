{
  description = "videocall-rs - WebTransport video calling platform";

  inputs = {
    # Pinned nixpkgs with pre-built:
    #   cargo-leptos        = 0.2.42 (0.2.x line, compatible with leptos 0.5.x)
    #   wasm-bindgen-cli    = 0.2.100 (exact match for Cargo.toml's =0.2.100)
    nixpkgs.url =
      "github:NixOS/nixpkgs/ee09932cedcef15aaf476f9343d1dea2cb77e261";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustMinimal = pkgs.rust-bin.nightly."2024-11-01".minimal.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        rustDev = pkgs.rust-bin.nightly."2024-11-01".default.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        commonBuildInputs = [
          pkgs.cargo-leptos
          pkgs.wasm-bindgen-cli_0_2_100
          pkgs.nodejs_20
          pkgs.binaryen
          pkgs.pkg-config
          pkgs.openssl
          pkgs.git
        ];

        commonEnv = {
          LEPTOS_HASH_FILES = "false";
          LEPTOS_TAILWIND_VERSION = "v3.4.17";
        };
      in
      {
        # Minimal shell for Docker and CI builds (no docs, no rust-analyzer)
        devShells.leptos-website = pkgs.mkShell (commonEnv // {
          nativeBuildInputs = [ rustMinimal ] ++ commonBuildInputs;
        });

        # Full shell for local development (includes rust-analyzer, docs)
        devShells.leptos-website-dev = pkgs.mkShell (commonEnv // {
          nativeBuildInputs = [ rustDev ] ++ commonBuildInputs;
        });

        devShells.default = self.devShells.${system}.leptos-website-dev;
      }
    );
}
