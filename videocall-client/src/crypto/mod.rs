pub mod aes;
pub mod rsa;

// Adding a compatibility layer for diagnostics
pub use self::aes::Aes128State as AesState;

// Simple function to help with decryption in diagnostics
pub fn decrypt(data: &[u8], aes_state: &AesState) -> Result<Vec<u8>, String> {
    aes_state.decrypt(data).map_err(|e| e.to_string())
}
