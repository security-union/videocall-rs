//! User identity byte conversion utilities.
//!
//! User IDs are stored as raw UTF-8 bytes in protobuf `bytes` fields.
//! This preserves the original identity string (email, guest ID, etc.)
//! while using the more efficient `bytes` wire type.

/// Convert an identity string to bytes for proto fields.
pub fn to_user_id_bytes(identity: &str) -> Vec<u8> {
    identity.as_bytes().to_vec()
}

/// Convert proto user_id bytes back to a string for display/logging.
pub fn user_id_bytes_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

/// Check if the given bytes represent the system user.
pub fn is_system_user(bytes: &[u8]) -> bool {
    bytes == crate::SYSTEM_USER_ID.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_output() {
        let a = to_user_id_bytes("alice@example.com");
        let b = to_user_id_bytes("alice@example.com");
        assert_eq!(a, b);
    }

    #[test]
    fn different_inputs_differ() {
        let a = to_user_id_bytes("alice@example.com");
        let b = to_user_id_bytes("bob@example.com");
        assert_ne!(a, b);
    }

    #[test]
    fn roundtrip_display() {
        let input = "test@example.com";
        let bytes = to_user_id_bytes(input);
        let display = user_id_bytes_to_string(&bytes);
        assert_eq!(display, input);
    }

    #[test]
    fn system_user_matches() {
        let bytes = to_user_id_bytes(crate::SYSTEM_USER_ID);
        assert!(is_system_user(&bytes));
    }

    #[test]
    fn non_system_user_does_not_match() {
        let bytes = to_user_id_bytes("alice@example.com");
        assert!(!is_system_user(&bytes));
    }

    #[test]
    fn non_utf8_fallback() {
        let display = user_id_bytes_to_string(&[0xde, 0xad, 0xbe, 0xef]);
        // from_utf8_lossy replaces invalid bytes with replacement character
        assert!(!display.is_empty());
    }
}
