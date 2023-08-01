use anyhow::anyhow;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

pub struct Aes128State {
    key: [u8; 16],
    iv: [u8; 16],
}

impl Default for Aes128State {
    fn default() -> Self {
        let mut key = [0u8; 16];
        getrandom::getrandom(&mut key).unwrap();
        let mut iv = [0u8; 16];
        getrandom::getrandom(&mut iv).unwrap();
        Aes128State { key, iv }
    }
}

impl Aes128State {
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
        let aes = Aes128State::default();
        let data = aes.encrypt(b"hello world").unwrap();
        let data2 = aes.decrypt(&data).unwrap();
        assert_eq!(data2, b"hello world");
    }
}
