//! Verifying that what arrived is what was stored.
//!
//! **Why not ETag.** The obvious-looking check is comparing the ETag to an MD5
//! of the file, and it is wrong: a multipart object's ETag is an MD5 of the
//! concatenated part digests plus a part count, not of the content. Comparing
//! it to a file digest fails on every multipart object — which is every large
//! one, the exact case worth checking. PLAN §4 flagged this from the start.
//!
//! So the check uses `x-amz-checksum-crc32`, which S3 computes over the content
//! and reports in base64. Providers that predate checksums, and objects
//! uploaded without one, report nothing — that is [`Verification::Unavailable`]
//! rather than a failure, because "we could not check" and "it is corrupt" call
//! for very different words to the user.

use std::path::Path;

use anyhow::{Context, Result};

/// What checking a downloaded file concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    /// Content matches what the server reported.
    Ok,
    /// The server reported a checksum and the bytes on disk do not match it.
    Mismatch,
    /// No checksum to compare against. Not a failure, and not a guarantee.
    Unavailable,
}

/// Base64 alphabet, written out because pulling a dependency for eight lines of
/// encoding is not worth the supply chain.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes exactly four bytes, which is all a CRC32 ever needs.
fn base64_crc32(value: u32) -> String {
    let bytes = value.to_be_bytes();
    let mut out = String::with_capacity(8);

    // 4 bytes is one full 3-byte group plus a 1-byte remainder.
    let group = [bytes[0], bytes[1], bytes[2]];
    let triple = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
    for shift in [18, 12, 6, 0] {
        out.push(B64[((triple >> shift) & 0x3f) as usize] as char);
    }

    let remainder = u32::from(bytes[3]) << 16;
    out.push(B64[((remainder >> 18) & 0x3f) as usize] as char);
    out.push(B64[((remainder >> 12) & 0x3f) as usize] as char);
    out.push('=');
    out.push('=');
    out
}

/// CRC32 of a file, in the base64 form S3 reports.
///
/// Reads in chunks: a downloaded file can be larger than memory, and the whole
/// point of this check is the large ones.
pub fn crc32_of_file(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("mở {} để kiểm checksum", path.display()))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("đọc {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(base64_crc32(hasher.finalize()))
}

/// Compares a downloaded file against what the server said.
pub fn verify(path: &Path, expected: Option<&str>) -> Result<Verification> {
    let Some(expected) = expected else {
        return Ok(Verification::Unavailable);
    };
    let actual = crc32_of_file(path)?;
    Ok(if actual == expected {
        Verification::Ok
    } else {
        Verification::Mismatch
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_known_value_for_check() {
        // "123456789" has CRC32 0xCBF43926 by definition in every CRC32 test
        // vector, so this pins the algorithm rather than trusting the crate.
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(b"123456789");
        assert_eq!(hasher.finalize(), 0xCBF4_3926);

        // And the base64 of those four bytes is what S3 puts in the header.
        assert_eq!(base64_crc32(0xCBF4_3926), "y/Q5Jg==");
    }

    #[test]
    fn base64_pads_a_four_byte_value_correctly() {
        assert_eq!(base64_crc32(0), "AAAAAA==");
        assert_eq!(base64_crc32(0xFFFF_FFFF), "/////w==");
        // Two padding characters always, because four bytes is never a whole
        // number of 3-byte groups.
        assert!(base64_crc32(0x1234_5678).ends_with("=="));
        assert_eq!(base64_crc32(0x1234_5678).len(), 8);
    }

    fn temp_file(contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "s3b-checksum-{}-{}",
            std::process::id(),
            contents.len()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn verify_distinguishes_matching_corrupt_and_unknown() {
        let path = temp_file(b"123456789");
        let good = "y/Q5Jg==";

        assert_eq!(verify(&path, Some(good)).unwrap(), Verification::Ok);

        // A different checksum means the bytes on disk are not what was stored.
        assert_eq!(
            verify(&path, Some("AAAAAA==")).unwrap(),
            Verification::Mismatch
        );

        // No checksum reported is not a failure. Treating it as one would fail
        // every download from a provider that does not implement checksums.
        assert_eq!(verify(&path, None).unwrap(), Verification::Unavailable);

        _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_file_still_has_a_checksum() {
        let path = temp_file(b"");
        // CRC32 of nothing is 0; a zero-byte object is a legitimate object and
        // must not be reported as unverifiable.
        assert_eq!(crc32_of_file(&path).unwrap(), "AAAAAA==");
        _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_larger_than_one_chunk_hashes_the_whole_thing() {
        // Bigger than the 1 MiB read buffer, so the chunk loop actually runs
        // more than once — a hasher fed only the first chunk would still look
        // correct on every small test.
        let big: Vec<u8> = (0..3_000_000).map(|i| (i % 251) as u8).collect();
        let path = temp_file(&big);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&big);
        assert_eq!(crc32_of_file(&path).unwrap(), base64_crc32(hasher.finalize()));

        _ = std::fs::remove_file(&path);
    }
}
