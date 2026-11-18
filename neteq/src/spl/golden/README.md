# NetEQ golden generators

These C++ programs compile the real WebRTC NetEq and signal-processing sources
(generic C, no SIMD) and print the hashes that the Rust tests commit as goldens.
`build.sh` feeds `spl/golden_tests.rs`, `build_wp2a.sh` feeds `neteq/wp2a_golden_tests.rs`,
and `build_wp2b.sh` feeds the WP2b tests.
The WebRTC checkout is the pinned `WEBRTC_SRC` from the parity Nix shell, so nothing
depends on a home-directory clone. Outputs are built in a temp dir and removed.

    nix-shell neteq/parity/shell.nix --run 'neteq/src/spl/golden/build.sh'
    nix-shell neteq/parity/shell.nix --run 'neteq/src/spl/golden/build_wp2a.sh'
    nix-shell neteq/parity/shell.nix --run 'neteq/src/spl/golden/build_wp2b.sh'
