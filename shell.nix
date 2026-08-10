# `nix-shell` — default dev shell. Named shells: `nix-shell -A shells.backend-dev`
# (see nix/shells.nix for the roster).
(import ./default.nix { }).shells.default
