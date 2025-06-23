/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

pub mod aes;
pub mod rsa;

// Adding a compatibility layer for diagnostics
pub use self::aes::Aes128State as AesState;

// Simple function to help with decryption in diagnostics
pub fn decrypt(data: &[u8], aes_state: &AesState) -> Result<Vec<u8>, String> {
    aes_state.decrypt(data).map_err(|e| e.to_string())
}
