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

use anyhow::anyhow;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use rand::RngCore;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aes128State {
    pub enabled: bool,
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

impl Aes128State {
    pub fn new(enabled: bool) -> Self {
        if enabled {
            Self::new_random()
        } else {
            Self {
                enabled,
                key: [0u8; 16],
                iv: [0u8; 16],
            }
        }
    }

    fn new_random() -> Self {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut iv);
        Self {
            enabled: true,
            key,
            iv,
        }
    }

    pub fn from_vecs(key: Vec<u8>, iv: Vec<u8>, enabled: bool) -> Self {
        let mut key_arr = [0u8; 16];
        let mut iv_arr = [0u8; 16];
        key_arr.copy_from_slice(&key);
        iv_arr.copy_from_slice(&iv);
        Self {
            enabled,
            key: key_arr,
            iv: iv_arr,
        }
    }

    pub fn encrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if !self.enabled {
            // XXX: Don't make a new copy of data.
            return Ok(data.to_vec());
        }
        let cipher =
            Aes128CbcEnc::new_from_slices(&self.key, &self.iv).map_err(|e| anyhow!("{e}"))?;
        Ok(cipher.encrypt_padded_vec_mut::<Pkcs7>(data))
    }

    pub fn decrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if !self.enabled {
            // XXX: Don't make a new copy of data.
            return Ok(data.to_vec());
        }
        let decipher = Aes128CbcDec::new_from_slices(&self.key, &self.iv)
            .map_err(|e| anyhow!("Decryptor Initialization error! {e}"))?;
        decipher
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(|e| anyhow!("Decrypt error! {e}"))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_aes() {
        let aes = Aes128State::new(true);
        let data = aes.encrypt(b"hello world").unwrap();
        let data2 = aes.decrypt(&data).unwrap();
        assert_eq!(data2, b"hello world");
    }

    #[wasm_bindgen_test]
    fn test_aes_large_payload() {
        let aes = Aes128State::new(true);
        let mut data = Vec::new();
        for _ in 0..1000 {
            data.extend_from_slice(b"hello world");
        }
        let enc_data = aes.encrypt(&data).unwrap();
        let data2 = aes.decrypt(&enc_data).unwrap();
        assert_eq!(data2, data);
    }

    #[wasm_bindgen_test]
    fn test_aes_disabled() {
        let aes = Aes128State::new(false);
        let mut data = Vec::new();
        for _ in 0..1000 {
            data.extend_from_slice(b"hello world");
        }
        let enc_data = aes.encrypt(&data).unwrap();
        let data2 = aes.decrypt(&enc_data).unwrap();
        assert_eq!(data2, data);
    }
}
