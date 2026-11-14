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

/// The chunk mapping of a blob: the ONE function every transport (ordinary
/// push, headless preload, bootstrap, staged, release upload) and every local
/// re-mapping uses, so a blob's mapping is a pure function of its bytes:
///
/// - zero bytes: no chunks (see [`chunk_content`]);
/// - below [`crate::LARGE_FILE_THRESHOLD`]: one chunk covering the whole
///   blob, whose hash is therefore the blob hash (an inline blob on the wire
///   is this same mapping);
/// - otherwise: FastCDC ([`stream_chunk_content`]).
///
/// Two versions of a large file therefore share every chunk an edit did not
/// touch, whichever transport carried either version. `size` selects the
/// shape and is verified: the iterator ends with an error when `reader` does
/// not yield exactly `size` bytes. Memory is bounded by one chunk (at most
/// the threshold, or FastCDC's maximum).
pub fn blob_chunk_mapping<R: Read>(size: u64, reader: R) -> BlobChunkMapping<R> {
    let shape = if size == 0 {
        MappingShape::Empty(reader)
    } else if size < crate::LARGE_FILE_THRESHOLD {
        MappingShape::Whole(reader)
    } else {
        MappingShape::ContentDefined(fastcdc::v2020::StreamCDC::new(
            reader,
            MIN_CHUNK_SIZE,
            AVG_CHUNK_SIZE,
            MAX_CHUNK_SIZE,
        ))
    };
    BlobChunkMapping {
        shape: Some(shape),
        size,
        seen: 0,
    }
}

enum MappingShape<R: Read> {
    Empty(R),
    Whole(R),
    ContentDefined(fastcdc::v2020::StreamCDC<R>),
}

/// Iterator returned by [`blob_chunk_mapping`].
pub struct BlobChunkMapping<R: Read> {
    shape: Option<MappingShape<R>>,
    size: u64,
    seen: u64,
}

impl<R: Read> BlobChunkMapping<R> {
    fn size_mismatch(&self) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "blob content is {} bytes, not the declared {}",
                self.seen, self.size
            ),
        )
    }

    fn finish(&mut self) -> Option<std::io::Result<(ChunkInfo, Vec<u8>)>> {
        self.shape = None;
        (self.seen != self.size).then(|| Err(self.size_mismatch()))
    }
}

impl<R: Read> Iterator for BlobChunkMapping<R> {
    type Item = std::io::Result<(ChunkInfo, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.shape.take()? {
            MappingShape::Empty(mut reader) => {
                let mut probe = [0u8; 1];
                match reader.read(&mut probe) {
                    Ok(read) => {
                        self.seen = read as u64;
                        self.finish()
                    }
                    Err(error) => Some(Err(error)),
                }
            }
            MappingShape::Whole(reader) => {
                // One byte past the declared size is enough to detect a
                // longer source without buffering it.
                let mut data = Vec::with_capacity(self.size as usize);
                if let Err(error) = reader.take(self.size + 1).read_to_end(&mut data) {
                    return Some(Err(error));
                }
                self.seen = data.len() as u64;
                if self.seen != self.size {
                    return Some(Err(self.size_mismatch()));
                }
                let info = ChunkInfo {
                    hash: hash_bytes(&data),
                    offset: 0,
                    // Below LARGE_FILE_THRESHOLD, far inside u32.
                    length: data.len() as u32,
                };
                Some(Ok((info, data)))
            }
            MappingShape::ContentDefined(mut chunker) => match chunker.next() {
                None => self.finish(),
                Some(Err(error)) => Some(Err(std::io::Error::other(error))),
                Some(Ok(chunk)) => {
                    let length = match u32::try_from(chunk.data.len()) {
                        Ok(length) => length,
                        Err(error) => return Some(Err(std::io::Error::other(error))),
                    };
                    self.seen += u64::from(length);
                    self.shape = Some(MappingShape::ContentDefined(chunker));
                    Some(Ok((
                        ChunkInfo {
                            hash: hash_bytes(&chunk.data),
                            offset: chunk.offset,
                            length,
                        },
                        chunk.data,
                    )))
                }
            },
        }
    }
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

    fn mapping(content: &[u8]) -> Vec<(ChunkInfo, Vec<u8>)> {
        blob_chunk_mapping(content.len() as u64, content)
            .collect::<std::io::Result<_>>()
            .unwrap()
    }

    fn pseudo_random(len: usize, mut state: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state & 0xFF) as u8
            })
            .collect()
    }

    #[test]
    fn blob_mapping_is_empty_whole_or_content_defined_by_size_alone() {
        assert!(mapping(&[]).is_empty());

        // Below the threshold: one chunk that IS the blob, even where FastCDC
        // alone would have cut the content into several chunks.
        let small = pseudo_random(3 * 1024 * 1024, 7);
        assert!(chunk_content(&small).len() > 1);
        let whole = mapping(&small);
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].0.hash, hash_bytes(&small));
        assert_eq!(whole[0].0.offset, 0);
        assert_eq!(whole[0].0.length as usize, small.len());

        // At and above the threshold: exactly the FastCDC mapping, never one
        // chunk covering the whole blob.
        for extra in [0usize, 1, 777_777] {
            let large = pseudo_random(crate::LARGE_FILE_THRESHOLD as usize + extra, 11);
            let chunks = mapping(&large);
            let expected = chunk_content(&large);
            assert!(chunks.len() > 1);
            assert_eq!(chunks.len(), expected.len());
            for ((info, bytes), (expected_info, expected_bytes)) in chunks.iter().zip(&expected) {
                assert_eq!(info.hash, expected_info.hash);
                assert_eq!(info.offset, expected_info.offset);
                assert_eq!(info.length, expected_info.length);
                assert_eq!(bytes, expected_bytes);
                assert!(info.length as usize <= MAX_CHUNK_SIZE);
            }
        }
    }

    #[test]
    fn an_in_place_edit_of_a_large_blob_changes_only_the_chunks_it_touches() {
        let base = pseudo_random(24 * 1024 * 1024, 0xDEAD_BEEF);
        let mut edited = base.clone();
        for byte in &mut edited[12_000_000..12_000_008] {
            *byte ^= 0xFF;
        }
        let before: std::collections::HashSet<_> = mapping(&base)
            .into_iter()
            .map(|(info, _)| info.hash)
            .collect();
        let after = mapping(&edited);
        let changed = after
            .iter()
            .filter(|(info, _)| !before.contains(&info.hash))
            .count();
        assert!(after.len() >= 6);
        assert!(
            (1..=2).contains(&changed),
            "an 8-byte edit re-mapped {changed} of {} chunks",
            after.len()
        );
    }

    #[test]
    fn blob_mapping_rejects_a_source_that_is_not_the_declared_size() {
        let content = pseudo_random(1024, 3);
        for declared in [0u64, 1023, 1025] {
            let result: std::io::Result<Vec<_>> =
                blob_chunk_mapping(declared, content.as_slice()).collect();
            assert!(result.is_err(), "declared {declared} was accepted");
        }
        let large = pseudo_random(crate::LARGE_FILE_THRESHOLD as usize + 5, 5);
        for declared in [large.len() as u64 - 1, large.len() as u64 + 1] {
            let result: std::io::Result<Vec<_>> =
                blob_chunk_mapping(declared, large.as_slice()).collect();
            assert!(result.is_err(), "declared {declared} was accepted");
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
