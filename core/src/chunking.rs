use crate::{hash_bytes, Hash};
use fastcdc::v2020::FastCDC;
use serde::{Deserialize, Serialize};
use std::io::Read;

/// zstd frame magic (`0xFD2FB528`, little-endian). A stored chunk starting with
/// this *might* be zstd-compressed; the hash check in [`chunk_decode`] confirms.
#[cfg(feature = "local-repo")]
const CHUNK_ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

#[cfg(feature = "local-repo")]
const CHUNK_ZSTD_LEVEL: i32 = 3;

/// Compress a chunk's bytes for at-rest storage / direct (presigned) upload if
/// zstd actually shrinks them; otherwise return them unchanged. Chunks are
/// content-addressed by the hash of their *plaintext*, so this is transparent.
/// The Oak server applies the identical transform on its `upload_chunk` path —
/// both sides must agree so [`chunk_decode`] can detect either form.
#[cfg(feature = "local-repo")]
pub fn chunk_encode(content: &[u8]) -> Vec<u8> {
    match zstd::encode_all(content, CHUNK_ZSTD_LEVEL) {
        Ok(z) if z.len() < content.len() => z,
        _ => content.to_vec(),
    }
}

/// Inverse of [`chunk_encode`], safe to run on bytes from any source (presigned
/// R2 GET, CDN, or an already-decompressed inline response): if `bytes` is a
/// zstd frame that decompresses to content whose hash equals `expected`, return
/// the decompressed content; otherwise return `bytes` unchanged. Raw/legacy
/// bytes pass through, so callers can apply it uniformly.
#[cfg(feature = "local-repo")]
pub fn chunk_decode(bytes: Vec<u8>, expected: &Hash) -> Vec<u8> {
    if bytes.len() >= 4 && bytes[..4] == CHUNK_ZSTD_MAGIC {
        if let Ok(d) = zstd::decode_all(&bytes[..]) {
            if &hash_bytes(&d) == expected {
                return d;
            }
        }
    }
    bytes
}

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
///
/// **Zero-byte content yields zero chunks**, not one empty chunk. Callers
/// relied on that implicitly long before it was written down: push only
/// chunks blobs at or above `LARGE_FILE_THRESHOLD`, so the empty blob never
/// reaches the chunk pipeline and is advertised with an empty `chunks` list.
/// Fetch paths must therefore treat "no chunk refs" as an error *except* for
/// the empty blob, whose bytes are implied by its hash (see
/// `crate::ensure_empty_blob`).
pub fn chunk_content(content: &[u8]) -> Vec<(ChunkInfo, Vec<u8>)> {
    let chunker = FastCDC::new(content, MIN_CHUNK_SIZE, AVG_CHUNK_SIZE, MAX_CHUNK_SIZE);
    chunker
        .map(|chunk| {
            let data = content[chunk.offset..chunk.offset + chunk.length].to_vec();
            let hash = hash_bytes(&data);
            let length = u32::try_from(chunk.length)
                .expect("FastCDC chunk length must fit in u32; check MAX_CHUNK_SIZE");
            let info = ChunkInfo {
                hash,
                offset: chunk.offset as u64,
                length,
            };
            (info, data)
        })
        .collect()
}

/// Content-defined chunking over a bounded-memory reader. Its boundaries are
/// byte-for-byte identical to [`chunk_content`], while retaining at most the
/// FastCDC maximum chunk window and one emitted chunk in memory.
pub fn stream_chunk_content<R: Read>(
    reader: R,
) -> impl Iterator<Item = std::io::Result<(ChunkInfo, Vec<u8>)>> {
    fastcdc::v2020::StreamCDC::new(reader, MIN_CHUNK_SIZE, AVG_CHUNK_SIZE, MAX_CHUNK_SIZE).map(
        |result| {
            let chunk = result.map_err(std::io::Error::other)?;
            let length = u32::try_from(chunk.data.len()).map_err(std::io::Error::other)?;
            let info = ChunkInfo {
                hash: hash_bytes(&chunk.data),
                offset: chunk.offset,
                length,
            };
            Ok((info, chunk.data))
        },
    )
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

    /// Zero-byte content chunks to *nothing* — not to one empty chunk. Every
    /// blob-fetch path branches on this: an empty `chunks` list is normally
    /// an unreachable-bytes error, and only the empty blob is allowed to have
    /// one. If FastCDC ever started emitting a degenerate zero-length chunk,
    /// those branches would silently change meaning.
    #[test]
    fn empty_content_produces_no_chunks() {
        assert!(chunk_content(&[]).is_empty());
        assert!(reassemble_chunks(&[]).is_empty());
    }

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
    fn streamed_chunking_matches_the_in_memory_mapping() {
        let content: Vec<u8> = (0..7 * 1024 * 1024)
            .map(|i| ((i * 11 + 29) % 251) as u8)
            .collect();
        let expected = chunk_content(&content);
        let actual: Vec<_> = stream_chunk_content(std::io::Cursor::new(&content))
            .collect::<std::io::Result<_>>()
            .unwrap();

        assert_eq!(actual.len(), expected.len());
        for ((actual_info, actual_bytes), (expected_info, expected_bytes)) in
            actual.iter().zip(&expected)
        {
            assert_eq!(actual_info.hash, expected_info.hash);
            assert_eq!(actual_info.offset, expected_info.offset);
            assert_eq!(actual_info.length, expected_info.length);
            assert_eq!(actual_bytes, expected_bytes);
        }
    }

    #[cfg(feature = "local-repo")]
    #[test]
    fn chunk_codec_round_trips_and_passes_raw_through() {
        // Compressible content shrinks and round-trips.
        let content = "repeated line\n".repeat(300).into_bytes();
        let hash = hash_bytes(&content);
        let stored = chunk_encode(&content);
        assert!(stored.len() < content.len());
        assert_eq!(chunk_decode(stored, &hash), content);

        // Legacy raw bytes (no compression) decode unchanged.
        let raw = b"a short raw chunk".to_vec();
        let raw_hash = hash_bytes(&raw);
        assert_eq!(chunk_decode(raw.clone(), &raw_hash), raw);
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
