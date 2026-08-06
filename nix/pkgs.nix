# The host/Linux package-set split — the load-bearing file of the no-flake
# build (see docs/nix-architecture.md).
#
# `pkgs`       — native package set for the machine running nix-build. Runs
#                tools, assembles Docker images (dockerTools), provides the
#                Rust toolchains (rust-overlay) and the wasm build stack.
# `pkgsLinux`  — Linux (musl) package set for image *payloads*. On Linux hosts
#                this is effectively pkgsCross-musl of the same arch (cheap);
#                on macOS it is a true cross set — which is what lets a Mac
#                build Linux images natively, no VM, per the nix.dev tutorial:
#                https://nix.dev/tutorials/nixos/building-and-running-docker-images.html
# `pkgsLinuxStatic` — same, but isStatic: used for the Rust service binaries so
#                images ship a single statically-linked ELF plus certs/tzdata.
{ inputs ? import ./tamal { }
, system ? builtins.currentSystem
}:
let
  linuxSystem = {
    aarch64-darwin = "aarch64-linux";
    x86_64-darwin = "x86_64-linux";
  }.${system} or system;

  muslTarget = {
    aarch64-linux = "aarch64-unknown-linux-musl";
    x86_64-linux = "x86_64-unknown-linux-musl";
  }.${linuxSystem};

  overlays = [ (import inputs.rust-overlay) ];

  # google-chrome is unfree; allowed so the dioxus-ui wasm component tests run
  # against a nix-pinned browser (see shells.nix), same as the old flake.
  config = {
    allowUnfreePredicate = pkg:
      (pkg.pname or (builtins.parseDrvName pkg.name).name) == "google-chrome";
  };

  pkgs = import inputs.nixpkgs { inherit system overlays config; };

  pkgsLinux = import inputs.nixpkgs {
    inherit system overlays config;
    crossSystem = { config = muslTarget; };
  };

  pkgsLinuxStatic = import inputs.nixpkgs {
    inherit system overlays config;
    crossSystem = {
      config = muslTarget;
      isStatic = true;
    };
  };

  # cargo-leptos 0.2.42 for the leptos-website shell lives in an older nixpkgs
  # tree (0.2.x required by leptos 0.5.x; newer trees ship 0.3.x).
  pkgsLeptos = import inputs.nixpkgs-leptos { inherit system; };
in
{
  inherit pkgs pkgsLinux pkgsLinuxStatic pkgsLeptos linuxSystem muslTarget;
}
