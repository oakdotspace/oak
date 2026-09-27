use crate::error::{OakError, Result};

/// Discriminator for the hash algorithm a `Hash` represents.
///
/// Inferred from the hex length so a `Hash` can flow between layers without
/// re-serialization. Backends decide which kinds they accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashKind {
    /// 32-byte BLAKE3 (Oak-native, 64 hex chars).
    Blake3,
    /// 20-byte SHA-1 (git default object id, 40 hex chars).
    GitSha1,
    /// 32-byte SHA-256 (git's sha256 object format, 64 hex chars).
    /// Indistinguishable from BLAKE3 by length alone — backends disambiguate by context.
    GitSha256,
    /// Not a recognized canonical hash length.
    Unknown,
}

/// A content hash. The underlying algorithm depends on the backend that produced it:
/// SQLite/Postgres backends produce BLAKE3; the git backend produces git OIDs (SHA-1
/// today, SHA-256 in repos using the sha256 object format).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hash(pub String);

impl Hash {
    /// Create a Hash from a hex string. Accepts 40 hex chars (git SHA-1) or 64 hex chars
    /// (BLAKE3 or git SHA-256), lowercase only — the canonical form every
    /// producer (BLAKE3 `to_hex`, git OIDs) emits, and the form hash preimages
    /// embed, so an uppercase alias of a stored hash is never valid.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let len_ok = hex.len() == 40 || hex.len() == 64;
        let chars_ok = hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !len_ok || !chars_ok {
            return Err(OakError::InvalidHash(hex.to_string()));
        }
        Ok(Hash(hex.to_string()))
    }

    /// Get the hex string representation
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Get a short version of the hash (first 12 characters)
    pub fn short(&self) -> &str {
        let end = self
            .0
            .char_indices()
            .map(|(idx, _)| idx)
            .nth(12)
            .unwrap_or(self.0.len());
        &self.0[..end]
    }

    /// Best-effort algorithm identification from hex length.
    ///
    /// 64-char hex collides between BLAKE3 and git SHA-256; this returns `Blake3` in that case.
    /// Callers that need to distinguish must use backend context, not this method.
    pub fn kind(&self) -> HashKind {
        match self.0.len() {
            40 => HashKind::GitSha1,
            64 => HashKind::Blake3,
            _ => HashKind::Unknown,
        }
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl serde::Serialize for Hash {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Hash {
    /// Deserialization is a trust boundary — wire payloads and stored JSON
    /// land here — so it enforces the same lowercase-hex shape as
    /// [`Hash::from_hex`]. Without it, arbitrary text (including the `\t`/`\n`
    /// bytes the canonical tree and commit preimages use as separators) could
    /// be injected into hash-position fields.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Hash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Hash arbitrary bytes using BLAKE3
pub fn hash_bytes(data: &[u8]) -> Hash {
    let hash = blake3::hash(data);
    Hash(hash.to_hex().to_string())
}

/// Hash a string
pub fn hash_string(s: &str) -> Hash {
    hash_bytes(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_determinism() {
        let data = b"hello world";
        let hash1 = hash_bytes(data);
        let hash2 = hash_bytes(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_different_inputs() {
        let hash1 = hash_bytes(b"hello");
        let hash2 = hash_bytes(b"world");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_length() {
        let hash = hash_bytes(b"test");
        assert_eq!(hash.as_str().len(), 64);
    }

    #[test]
    fn test_hash_from_hex_blake3() {
        let valid_hex = "a".repeat(64);
        let h = Hash::from_hex(&valid_hex).unwrap();
        assert_eq!(h.kind(), HashKind::Blake3);
    }

    #[test]
    fn test_hash_from_hex_git_sha1() {
        let valid_hex = "a".repeat(40);
        let h = Hash::from_hex(&valid_hex).unwrap();
        assert_eq!(h.kind(), HashKind::GitSha1);
    }

    #[test]
    fn test_hash_from_hex_invalid_length() {
        assert!(Hash::from_hex("abc").is_err());
        assert!(Hash::from_hex(&"a".repeat(50)).is_err());
    }

    #[test]
    fn test_hash_from_hex_invalid_chars() {
        let invalid_hex = "g".repeat(64);
        assert!(Hash::from_hex(&invalid_hex).is_err());
    }

    #[test]
    fn test_hash_short() {
        let hash = hash_bytes(b"test");
        assert_eq!(hash.short().len(), 12);
    }

    #[test]
    fn test_hash_short_handles_non_ascii_without_panicking() {
        let hash = Hash("é".repeat(20));
        assert_eq!(hash.short(), "éééééééééééé");
    }

    #[test]
    fn test_hash_kind_reports_unknown_length() {
        assert_eq!(Hash("abc".to_string()).kind(), HashKind::Unknown);
    }

    #[test]
    fn test_hash_from_hex_rejects_uppercase() {
        assert!(Hash::from_hex(&"A".repeat(64)).is_err());
        assert!(Hash::from_hex(&format!("{}F", "a".repeat(63))).is_err());
    }

    #[test]
    fn test_deserialize_valid_hex() {
        let blake3 = format!("\"{}\"", "a".repeat(64));
        let h: Hash = serde_json::from_str(&blake3).unwrap();
        assert_eq!(h.kind(), HashKind::Blake3);

        let sha1 = format!("\"{}\"", "0123456789abcdef0123".repeat(2));
        let h: Hash = serde_json::from_str(&sha1).unwrap();
        assert_eq!(h.kind(), HashKind::GitSha1);

        // Round-trip: a real hash serializes and deserializes to itself.
        let original = hash_bytes(b"round trip");
        let json = serde_json::to_string(&original).unwrap();
        let back: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn test_deserialize_rejects_junk() {
        let cases = [
            "\"\"".to_string(),
            "\"not a hash\"".to_string(),
            format!("\"{}\"", "a".repeat(63)), // short
            format!("\"{}\"", "a".repeat(65)), // long
            format!("\"{}\"", "A".repeat(64)), // uppercase
            format!("\"{}\"", "g".repeat(64)), // non-hex
            format!("\"{}\\t{}\"", "a".repeat(31), "a".repeat(32)), // embedded tab
            format!("\"{}\\n{}\"", "a".repeat(31), "a".repeat(32)), // embedded newline
        ];
        for json in cases {
            assert!(
                serde_json::from_str::<Hash>(&json).is_err(),
                "should reject {json}"
            );
        }
    }
}
