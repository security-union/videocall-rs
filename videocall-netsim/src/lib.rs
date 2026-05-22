//! Transport-layer network-condition simulator. Lifted from the synthetic
//! bot's `bot::netsim` so it can also run inside `wasm32-unknown-unknown`
//! (i.e. inside `videocall-client`) — that's how the bots-app browser bot
//! shapes its own outbound media to mimic a peer on a degraded network
//! while a human peer evaluates the result.
//!
//! See [`shim::NetSimShim`] for the per-direction shim and [`profiles`]
//! for the named impairment presets.

#![deny(missing_debug_implementations)]

pub mod profiles;
pub mod shim;

pub use profiles::{list_profiles, resolve_profile, PRESET_NAMES};
pub use shim::{Admission, Direction, NetSimShim, NetworkProfile};
