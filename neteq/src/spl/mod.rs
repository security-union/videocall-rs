// Generic-C port of WebRTC common_audio/signal_processing (PARITY_PLAN.md
// section 2.4): one Rust file per C file, `WebRtcSpl_FooBar` becomes
// `spl::foo_bar`. Integer overflow that wraps in C uses `wrapping_*` here.
//
// `golden/` holds the C++ generator for the golden-hash tests: it builds the
// real WebRTC sources and prints the hashes `golden_tests.rs` checks.

//! Fixed-point signal processing library (SPL).

// The C signatures are kept argument for argument.
#![allow(clippy::too_many_arguments)]

pub mod auto_correlation;
pub mod copy_set_operations;
pub mod cross_correlation;
pub mod division_operations;
pub mod dot_product_with_scale;
pub mod downsample_fast;
pub mod energy;
pub mod filter_ar;
pub mod filter_ar_fast_q12;
pub mod filter_ma_fast_q12;
pub mod get_hanning_window;
pub mod levinson_durbin;
pub mod min_max_operations;
pub mod randomization_functions;
pub mod signal_processing_library;
pub mod spl_inl;
pub mod spl_sqrt;
pub mod spl_sqrt_floor;
pub mod vector_operations;
pub mod vector_scaling_operations;

pub use auto_correlation::*;
pub use copy_set_operations::*;
pub use cross_correlation::*;
pub use division_operations::*;
pub use dot_product_with_scale::*;
pub use downsample_fast::*;
pub use energy::*;
pub use filter_ar::*;
pub use filter_ar_fast_q12::*;
pub use filter_ma_fast_q12::*;
pub use get_hanning_window::*;
pub use levinson_durbin::*;
pub use min_max_operations::*;
pub use randomization_functions::*;
pub use signal_processing_library::*;
pub use spl_inl::*;
pub use spl_sqrt::*;
pub use spl_sqrt_floor::*;
pub use vector_operations::*;
pub use vector_scaling_operations::*;

#[cfg(test)]
pub(crate) mod golden_tests;
#[cfg(test)]
pub(crate) mod test_util;
