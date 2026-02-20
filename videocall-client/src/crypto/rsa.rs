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

use crate::constants::RSA_BITS;
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

#[derive(Clone, Debug, PartialEq)]
pub struct RsaWrapper {
    pub enabled: bool,
    pub pub_key: RsaPublicKey,
    key: RsaPrivateKey,
}

impl RsaWrapper {
    pub fn new(enabled: bool) -> Self {
        if enabled {
            Self::new_random()
        } else {
            // When disabled, create a minimal valid key that won't be used
            let mut rng = rand::thread_rng();
            // Use minimum possible bits (1024) to minimize overhead
            let dummy_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
            Self {
                enabled: false, // explicitly set to false
                pub_key: dummy_key.to_public_key(),
                key: dummy_key,
            }
        }
    }

    fn new_random() -> Self {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, RSA_BITS).unwrap();
        let pub_key = key.to_public_key();
        Self {
            enabled: true,
            key,
            pub_key,
        }
    }

    pub fn decrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if !self.enabled {
            return Ok(data.to_vec());
        }
        Ok(self.key.decrypt(Pkcs1v15Encrypt, data)?)
    }

    pub fn encrypt_with_key(&self, data: &[u8], key: &RsaPublicKey) -> anyhow::Result<Vec<u8>> {
        if !self.enabled {
            return Ok(data.to_vec());
        }
        let mut rng = rand::thread_rng();
        Ok(key.encrypt(&mut rng, Pkcs1v15Encrypt, data)?)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn test_rsa_thread_rng() {
        let key = RsaWrapper::new(true);
        let data = b"hello world";
        let encrypted = key.encrypt_with_key(data, &key.pub_key).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn test_rsa_disabled() {
        let key = RsaWrapper::new(false);
        assert!(!key.enabled); // verify it's actually disabled
        let data = b"hello world";
        let encrypted = key.encrypt_with_key(data, &key.pub_key).unwrap();
        assert_eq!(data, &encrypted[..]); // verify no encryption happened
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }
}
