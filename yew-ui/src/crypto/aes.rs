use anyhow::anyhow;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use rand::RngCore;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aes128State {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

impl Aes128State {
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut iv);
        Aes128State { key, iv }
    }

    pub fn from_vecs(key: Vec<u8>, iv: Vec<u8>) -> Self {
        let mut key_arr = [0u8; 16];
        let mut iv_arr = [0u8; 16];
        key_arr.copy_from_slice(&key);
        iv_arr.copy_from_slice(&iv);
        Aes128State {
            key: key_arr,
            iv: iv_arr,
        }
    }

    pub fn encrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let cipher = Aes128CbcEnc::new_from_slices(&self.key, &self.iv)
            .map_err(|e| anyhow!("{}", e.to_string()))?;
        Ok(cipher.encrypt_padded_vec_mut::<Pkcs7>(data))
    }

    pub fn decrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let decipher = Aes128CbcDec::new_from_slices(&self.key, &self.iv)
            .map_err(|e| anyhow!("Decryptor Initialization error! {}", e.to_string()))?;
        decipher
            .decrypt_padded_vec_mut::<Pkcs7>(data)
            .map_err(|e| anyhow!("Decrypt error! {}", e.to_string()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_aes() {
        let aes = Aes128State::new();
        let data = aes.encrypt(b"hello world").unwrap();
        let data2 = aes.decrypt(&data).unwrap();
        assert_eq!(data2, b"hello world");
    }

    #[wasm_bindgen_test]
    fn test_aes_large_payload() {
        let aes = Aes128State::new();
        let mut data = Vec::new();
        for _ in 0..1000 {
            data.extend_from_slice(b"hello world");
        }
        let enc_data = aes.encrypt(&data).unwrap();
        let data2 = aes.decrypt(&enc_data).unwrap();
        assert_eq!(data2, data);
    }
}
