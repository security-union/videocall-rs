# CI/deploy surface: one attribute per publishable artifact.
#
#   $(nix-build release.nix -A images.websocket-server --no-out-link) | docker load
#
# Image attrs are streamLayeredImage *scripts*: running the result streams the
# image tarball to stdout (pipe into `docker load`). See docs/nix-architecture.md.
{ system ? builtins.currentSystem
, gitSha ? "unknown"
, gitBranch ? "nix"
, buildTimestamp ? "unknown"
}:
let
  d = import ./default.nix { inherit system gitSha gitBranch buildTimestamp; };
in
{
  inherit (d) packages images;
}
