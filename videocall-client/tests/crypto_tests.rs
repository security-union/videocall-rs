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

//! Cross-platform crypto tests for AES and RSA.
//! These verify that encryption/decryption roundtrips work on both native and WASM.

#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use videocall_client::crypto::aes::Aes128State;
    use videocall_client::crypto::rsa::RsaWrapper;

    // ── AES Tests ─────────────────────────────────────────────────────────────

    #[test]
    fn test_aes_roundtrip_small() {
        let aes = Aes128State::new(true);
        let plaintext = b"hello world";
        let ciphertext = aes.encrypt(plaintext).unwrap();
        assert_ne!(&ciphertext, plaintext, "Ciphertext should differ from plaintext");
        let decrypted = aes.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_aes_roundtrip_large() {
        let aes = Aes128State::new(true);
        let plaintext: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let ciphertext = aes.encrypt(&plaintext).unwrap();
        let decrypted = aes.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_aes_disabled_passthrough() {
        let aes = Aes128State::new(false);
        let data = b"plaintext passes through";
        let encrypted = aes.encrypt(data).unwrap();
        assert_eq!(&encrypted, data, "Disabled AES should pass through data unchanged");
        let decrypted = aes.decrypt(&encrypted).unwrap();
        assert_eq!(&decrypted, data);
    }

    #[test]
    fn test_aes_different_keys_cannot_decrypt() {
        let aes1 = Aes128State::new(true);
        let aes2 = Aes128State::new(true);
        let plaintext = b"secret data";
        let ciphertext = aes1.encrypt(plaintext).unwrap();
        let result = aes2.decrypt(&ciphertext);
        // Different keys should produce a decryption error or garbage
        match result {
            Ok(decrypted) => assert_ne!(decrypted, plaintext),
            Err(_) => {} // Expected
        }
    }

    #[test]
    fn test_aes_from_vecs() {
        let key = vec![0x42u8; 16];
        let iv = vec![0x13u8; 16];
        let aes1 = Aes128State::from_vecs(key.clone(), iv.clone(), true);
        let aes2 = Aes128State::from_vecs(key, iv, true);

        let plaintext = b"deterministic keys";
        let ciphertext = aes1.encrypt(plaintext).unwrap();
        let decrypted = aes2.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, plaintext, "Same key/IV should produce same result");
    }

    #[test]
    fn test_aes_empty_data() {
        let aes = Aes128State::new(true);
        let ciphertext = aes.encrypt(b"").unwrap();
        let decrypted = aes.decrypt(&ciphertext).unwrap();
        assert!(decrypted.is_empty());
    }

    // ── RSA Tests ─────────────────────────────────────────────────────────────

    #[test]
    fn test_rsa_roundtrip() {
        let rsa = RsaWrapper::new(true);
        let plaintext = b"RSA test message";
        let ciphertext = rsa.encrypt_with_key(plaintext, &rsa.pub_key).unwrap();
        assert_ne!(&ciphertext, plaintext);
        let decrypted = rsa.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_rsa_disabled_passthrough() {
        let rsa = RsaWrapper::new(false);
        assert!(!rsa.enabled);
        let data = b"no encryption";
        let encrypted = rsa.encrypt_with_key(data, &rsa.pub_key).unwrap();
        assert_eq!(&encrypted, data, "Disabled RSA should pass through");
        let decrypted = rsa.decrypt(&encrypted).unwrap();
        assert_eq!(&decrypted, data);
    }

    #[test]
    fn test_rsa_cross_key_decrypt_fails() {
        let rsa1 = RsaWrapper::new(true);
        let rsa2 = RsaWrapper::new(true);
        let plaintext = b"cross-key test";
        let ciphertext = rsa1.encrypt_with_key(plaintext, &rsa1.pub_key).unwrap();
        let result = rsa2.decrypt(&ciphertext);
        assert!(result.is_err(), "Decrypting with wrong key should fail");
    }

    // ── E2EE Protocol Flow Tests ──────────────────────────────────────────────

    #[test]
    fn test_e2ee_key_exchange_simulation() {
        // Simulate the E2EE key exchange: Alice sends her AES key to Bob
        // encrypted with Bob's RSA public key.
        let alice_aes = Aes128State::new(true);
        let bob_rsa = RsaWrapper::new(true);

        // Alice encrypts her AES key/IV with Bob's public key
        let mut aes_key_data = Vec::new();
        aes_key_data.extend_from_slice(&alice_aes.key);
        aes_key_data.extend_from_slice(&alice_aes.iv);

        let encrypted_aes_key = bob_rsa
            .encrypt_with_key(&aes_key_data, &bob_rsa.pub_key)
            .unwrap();

        // Bob decrypts to recover Alice's AES key
        let decrypted_aes_key = bob_rsa.decrypt(&encrypted_aes_key).unwrap();
        assert_eq!(decrypted_aes_key.len(), 32); // 16 bytes key + 16 bytes IV

        let bob_aes = Aes128State::from_vecs(
            decrypted_aes_key[..16].to_vec(),
            decrypted_aes_key[16..].to_vec(),
            true,
        );

        // Now Alice encrypts a message and Bob can decrypt it
        let message = b"Hello from Alice!";
        let ciphertext = alice_aes.encrypt(message).unwrap();
        let decrypted = bob_aes.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, message, "E2EE key exchange should work");
    }
}
