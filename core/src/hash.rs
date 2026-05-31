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
}

/// A content hash. The underlying algorithm depends on the backend that produced it:
/// SQLite/Postgres backends produce BLAKE3; the git backend produces git OIDs (SHA-1
/// today, SHA-256 in repos using the sha256 object format).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hash(pub String);

impl Hash {
    /// Create a Hash from a hex string. Accepts 40 hex chars (git SHA-1) or 64 hex chars
    /// (BLAKE3 or git SHA-256).
    pub fn from_hex(hex: &str) -> Result<Self> {
        let len_ok = hex.len() == 40 || hex.len() == 64;
        if !len_ok || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
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
        &self.0[..12.min(self.0.len())]
    }

    /// Best-effort algorithm identification from hex length.
    ///
    /// 64-char hex collides between BLAKE3 and git SHA-256; this returns `Blake3` in that case.
    /// Callers that need to distinguish must use backend context, not this method.
    pub fn kind(&self) -> HashKind {
        match self.0.len() {
            40 => HashKind::GitSha1,
            _ => HashKind::Blake3,
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
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Hash(s))
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
}
