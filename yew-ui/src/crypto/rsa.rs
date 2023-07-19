use crate::constants::RSA_BITS;
use rsa::{RsaPrivateKey, RsaPublicKey, Pkcs1v15Encrypt};

pub struct RsaWrapper {
    pub_key: RsaPublicKey,
    key: RsaPrivateKey,
    rng: rand::rngs::ThreadRng,
}

impl RsaWrapper {
    pub fn new() -> anyhow::Result<Self> {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, RSA_BITS)?;
        let pub_key = key.to_public_key();
        Ok(Self { key, pub_key, rng })
    }

    pub fn encrypt(&mut self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(self.pub_key.encrypt(&mut self.rng, Pkcs1v15Encrypt,&data,)?)
    }

    pub fn decrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(self.key.decrypt(Pkcs1v15Encrypt, &data)?)
    }
}

mod test {
    use wasm_bindgen_test::*;
    use super::*;

    #[wasm_bindgen_test]
    fn test_rsa_thread_rng() {
        let mut key = RsaWrapper::new().unwrap();
        let data = b"hello world";
        let encrypted = key.encrypt(data).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }
}
