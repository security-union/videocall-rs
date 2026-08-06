# videocall-rs — native Nix entry point (no flakes).
#
#   nix-shell                                 -> default dev shell
#   nix-build -A packages.websocket-server    -> a service binary (Linux musl)
#   nix-build release.nix -A images.…         -> Docker images (see release.nix)
#
# Inputs are pinned by nixtamal (nix/tamal/); see docs/nix-architecture.md.
{ inputs ? import ./nix/tamal { }
, system ? builtins.currentSystem
, gitSha ? "unknown"
, gitBranch ? "nix"
, buildTimestamp ? "unknown"
}:
let
  p = import ./nix/pkgs.nix { inherit inputs system; };
  rust = import ./nix/rust.nix { inherit p; };
  packages = import ./nix/packages.nix {
    inherit p rust gitSha gitBranch buildTimestamp;
  };
  images = import ./nix/images { inherit p packages; };
  devStack = import ./nix/dev-stack.nix { inherit p; };
  shells = import ./nix/shells.nix { inherit p rust devStack; };
in
{
  inherit shells packages images devStack;
  inherit (p) pkgs pkgsLinux pkgsLinuxStatic muslTarget;
}
