# Dev shell for the NetEQ parity harness. Reuses the repo's nixtamal-pinned
# nixpkgs (no flakes): `nix-shell neteq/parity/shell.nix`.
(import ../../default.nix { }).shells.neteq-parity
