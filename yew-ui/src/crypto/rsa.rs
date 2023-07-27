use crate::constants::RSA_BITS;
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

#[derive(Clone, Debug, PartialEq)]
pub struct RsaWrapper {
    pub_key: RsaPublicKey,
    key: RsaPrivateKey,
}

impl RsaWrapper {
    pub fn new() -> anyhow::Result<Self> {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, RSA_BITS)?;
        let pub_key = key.to_public_key();
        Ok(Self { key, pub_key })
    }

    pub fn encrypt(&mut self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(self.pub_key.encrypt(&mut self.rng, Pkcs1v15Encrypt, data)?)
    }

    pub fn decrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(self.key.decrypt(Pkcs1v15Encrypt, data)?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_rsa_thread_rng() {
        let mut key = RsaWrapper::new().unwrap();
        let data = b"hello world";
        let encrypted = key.encrypt(data).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }
}
