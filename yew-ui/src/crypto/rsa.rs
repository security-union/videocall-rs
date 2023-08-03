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
            let dummy_key = RsaPrivateKey::from_components(
                0u8.into(),
                0u8.into(),
                0u8.into(),
                [13u32.into(), 257u32.into()].to_vec(),
            )
            .unwrap();
            Self {
                enabled,
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
            // XXX: Don't make a new copy of data.
            return Ok(data.to_vec());
        }
        Ok(self.key.decrypt(Pkcs1v15Encrypt, data)?)
    }

    pub fn encrypt_with_key(&self, data: &[u8], key: &RsaPublicKey) -> anyhow::Result<Vec<u8>> {
        if !self.enabled {
            // XXX: Don't make a new copy of data.
            return Ok(data.to_vec());
        }
        let mut rng = rand::thread_rng();
        Ok(key.encrypt(&mut rng, Pkcs1v15Encrypt, data)?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_rsa_thread_rng() {
        let key = RsaWrapper::new(true);
        let data = b"hello world";
        let encrypted = key.encrypt_with_key(data, &key.pub_key).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }

    #[wasm_bindgen_test]
    fn test_rsa_disabled() {
        let key = RsaWrapper::new(false);
        let data = b"hello world";
        let encrypted = key.encrypt_with_key(data, &key.pub_key).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }
}
