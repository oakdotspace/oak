use crate::{hash_bytes, Hash};
use fastcdc::v2020::FastCDC;
use serde::{Deserialize, Serialize};

/// Minimum chunk size: 256 KB
const MIN_CHUNK_SIZE: usize = 256 * 1024;
/// Average chunk size: 1 MB
const AVG_CHUNK_SIZE: usize = 1024 * 1024;
/// Maximum chunk size: 4 MB
const MAX_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Metadata about a single chunk within a blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInfo {
    /// BLAKE3 hash of the chunk content
    pub hash: Hash,
    /// Byte offset in the original blob
    pub offset: u64,
    /// Chunk size in bytes
    pub length: u32,
}

/// Chunk file content using FastCDC with Gear hash rolling function.
///
/// Returns an ordered list of (ChunkInfo, chunk_data) pairs.
pub fn chunk_content(content: &[u8]) -> Vec<(ChunkInfo, Vec<u8>)> {
    let chunker = FastCDC::new(content, MIN_CHUNK_SIZE, AVG_CHUNK_SIZE, MAX_CHUNK_SIZE);
    chunker
        .map(|chunk| {
            let data = content[chunk.offset..chunk.offset + chunk.length].to_vec();
            let hash = hash_bytes(&data);
            let info = ChunkInfo {
                hash,
                offset: chunk.offset as u64,
                length: chunk.length as u32,
            };
            (info, data)
        })
        .collect()
}

/// Reassemble original content from ordered chunk data.
pub fn reassemble_chunks(chunk_data: &[&[u8]]) -> Vec<u8> {
    let total: usize = chunk_data.iter().map(|c| c.len()).sum();
    let mut result = Vec::with_capacity(total);
    for data in chunk_data {
        result.extend_from_slice(data);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_small_content() {
        // Content smaller than min chunk size produces a single chunk
        let content = vec![42u8; 100];
        let chunks = chunk_content(&content);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, content);
    }

    #[test]
    fn test_chunk_and_reassemble() {
        // 10 MB of pseudo-random data
        let content: Vec<u8> = (0..10 * 1024 * 1024)
            .map(|i| ((i * 7 + 13) % 256) as u8)
            .collect();
        let chunks = chunk_content(&content);
        assert!(chunks.len() > 1, "Should produce multiple chunks");

        let data_refs: Vec<&[u8]> = chunks.iter().map(|(_, d)| d.as_slice()).collect();
        let reassembled = reassemble_chunks(&data_refs);
        assert_eq!(reassembled, content);
    }

    #[test]
    fn test_chunk_determinism() {
        let content: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|i| ((i * 3 + 5) % 256) as u8)
            .collect();
        let chunks1 = chunk_content(&content);
        let chunks2 = chunk_content(&content);

        assert_eq!(chunks1.len(), chunks2.len());
        for (a, b) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(a.0.hash, b.0.hash);
            assert_eq!(a.0.offset, b.0.offset);
            assert_eq!(a.0.length, b.0.length);
        }
    }

    #[test]
    fn test_chunk_boundary_stability() {
        // Use a simple PRNG for varied content (xorshift32)
        let mut state: u32 = 0xDEAD_BEEF;
        let mut next = || -> u8 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xFF) as u8
        };

        let base: Vec<u8> = (0..5 * 1024 * 1024).map(|_| next()).collect();

        // Insert 1KB at the beginning — CDC should re-sync quickly
        let mut modified = vec![0u8; 1024];
        modified.extend_from_slice(&base);

        let base_chunks = chunk_content(&base);
        let mod_chunks = chunk_content(&modified);

        let base_hashes: std::collections::HashSet<_> = base_chunks
            .iter()
            .map(|(info, _)| info.hash.clone())
            .collect();
        let mod_hashes: std::collections::HashSet<_> = mod_chunks
            .iter()
            .map(|(info, _)| info.hash.clone())
            .collect();

        let shared = base_hashes.intersection(&mod_hashes).count();
        assert!(
            shared > 0,
            "CDC should share chunks between similar content (base={}, mod={}, shared={})",
            base_chunks.len(),
            mod_chunks.len(),
            shared
        );
    }

    #[test]
    fn test_chunk_hashes_are_valid() {
        let content: Vec<u8> = (0..1024 * 1024).map(|i| ((i * 17) % 256) as u8).collect();
        let chunks = chunk_content(&content);

        for (info, data) in &chunks {
            let expected_hash = hash_bytes(data);
            assert_eq!(info.hash, expected_hash);
            assert_eq!(info.length as usize, data.len());
        }
    }
}
