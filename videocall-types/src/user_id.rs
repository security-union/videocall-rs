//! User identity byte conversion utilities.
//!
//! User IDs are UUIDs stored as 16 raw bytes in protobuf `bytes` fields.
//! This provides compact, fixed-size identity representation with
//! efficient wire encoding.

use uuid::Uuid;

/// Convert a UUID to bytes for proto fields.
///
/// Returns the 16-byte big-endian representation of the UUID.
pub fn to_user_id_bytes(uuid: &Uuid) -> Vec<u8> {
    uuid.as_bytes().to_vec()
}

/// Parse 16 bytes back to a UUID.
///
/// Returns `None` if the slice is not exactly 16 bytes.
pub fn user_id_bytes_to_uuid(bytes: &[u8]) -> Option<Uuid> {
    Uuid::from_slice(bytes).ok()
}

/// Format user ID bytes as a human-readable string for display/logging.
///
/// If the bytes are a valid 16-byte UUID, returns the standard hyphenated
/// UUID string (e.g., `"550e8400-e29b-41d4-a716-446655440000"`).
/// Otherwise, falls back to a hex representation of the raw bytes.
pub fn user_id_bytes_to_string(bytes: &[u8]) -> String {
    match Uuid::from_slice(bytes) {
        Ok(uuid) => uuid.as_hyphenated().to_string(),
        Err(_) => bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>(),
    }
}

/// Parse a UUID from a string (e.g., from a JWT `sub` claim).
///
/// Accepts standard UUID formats: hyphenated, simple, URN, braced.
pub fn parse_user_id(s: &str) -> Result<Uuid, uuid::Error> {
    Uuid::parse_str(s)
}

/// Check if the given bytes represent the system user (nil UUID).
///
/// The system user is identified by the nil UUID (16 zero bytes),
/// used for server-generated messages such as meeting info.
pub fn is_system_user(bytes: &[u8]) -> bool {
    bytes == crate::SYSTEM_USER_ID.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_uuid_through_bytes() {
        let original = Uuid::new_v4();
        let bytes = to_user_id_bytes(&original);
        let recovered = user_id_bytes_to_uuid(&bytes).expect("should parse back to UUID");
        assert_eq!(original, recovered);
    }

    #[test]
    fn fixed_size_16_bytes() {
        let uuid = Uuid::new_v4();
        let bytes = to_user_id_bytes(&uuid);
        assert_eq!(bytes.len(), 16);
    }

    #[test]
    fn string_roundtrip() {
        let original = Uuid::new_v4();
        let bytes = to_user_id_bytes(&original);
        let display = user_id_bytes_to_string(&bytes);
        assert_eq!(display, original.as_hyphenated().to_string());
    }

    #[test]
    fn parse_roundtrip() {
        let input = "550e8400-e29b-41d4-a716-446655440000";
        let uuid = parse_user_id(input).expect("should parse valid UUID string");
        assert_eq!(uuid.as_hyphenated().to_string(), input);

        // Full roundtrip through bytes
        let bytes = to_user_id_bytes(&uuid);
        let recovered = user_id_bytes_to_uuid(&bytes).expect("should recover UUID from bytes");
        assert_eq!(recovered.as_hyphenated().to_string(), input);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_user_id("not-a-uuid").is_err());
    }

    #[test]
    fn parse_rejects_email() {
        assert!(parse_user_id("alice@example.com").is_err());
    }

    #[test]
    fn system_user_is_nil_uuid() {
        assert!(is_system_user(Uuid::nil().as_bytes()));
    }

    #[test]
    fn non_system_user() {
        assert!(!is_system_user(Uuid::new_v4().as_bytes()));
    }

    #[test]
    fn short_bytes_rejected() {
        assert!(user_id_bytes_to_uuid(&[0u8; 15]).is_none());
    }

    #[test]
    fn long_bytes_rejected() {
        assert!(user_id_bytes_to_uuid(&[0u8; 17]).is_none());
    }

    #[test]
    fn non_uuid_bytes_fallback_to_hex() {
        let bytes = [0xde, 0xad, 0xbe, 0xef];
        let display = user_id_bytes_to_string(&bytes);
        assert_eq!(display, "deadbeef");
    }

    #[test]
    fn nil_uuid_bytes_to_string() {
        let bytes = to_user_id_bytes(&Uuid::nil());
        let display = user_id_bytes_to_string(&bytes);
        assert_eq!(display, "00000000-0000-0000-0000-000000000000");
    }

    /// Wire format regression test: ensures PacketWrapper user_id is exactly
    /// 16 bytes (UUID) and not 36 bytes (string) or variable-length (email).
    #[test]
    fn packet_wrapper_user_id_is_16_byte_uuid() {
        use crate::protos::packet_wrapper::PacketWrapper;
        use protobuf::Message;

        let uuid = parse_user_id("550e8400-e29b-41d4-a716-446655440000").expect("valid UUID");
        let uuid_bytes = to_user_id_bytes(&uuid);
        assert_eq!(uuid_bytes.len(), 16, "UUID bytes must be exactly 16");

        // Build a PacketWrapper with UUID bytes
        let mut pkt = PacketWrapper::new();
        pkt.user_id = uuid_bytes.clone();

        // Serialize to protobuf wire format
        let wire = pkt.write_to_bytes().expect("serialize");

        // Deserialize back
        let parsed = PacketWrapper::parse_from_bytes(&wire).expect("deserialize");

        // Assert the user_id field is exactly 16 bytes
        assert_eq!(
            parsed.user_id.len(),
            16,
            "user_id on the wire must be 16 bytes, got {}",
            parsed.user_id.len()
        );

        // Assert the UUID round-trips correctly
        let recovered = user_id_bytes_to_uuid(&parsed.user_id).expect("should parse back to UUID");
        assert_eq!(recovered, uuid);
    }
}
