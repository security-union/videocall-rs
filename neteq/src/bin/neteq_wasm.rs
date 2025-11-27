#![cfg_attr(target_arch = "wasm32", no_main)]

#[cfg(target_arch = "wasm32")]
use neteq as _;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("neteq_wasm is only compiled for wasm32 target");
}
