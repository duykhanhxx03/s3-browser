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
    // `encode_crc32`, not `crc32_base64`: the digest is already computed, and
    // hashing it again yields a value that matches nothing.
    Ok(s3core::encode_crc32(hasher.finalize()))
}

/// Whether a reported checksum covers the whole object or is composed from its
/// parts.
///
/// A multipart upload's checksum is a CRC32 over the concatenated part
/// checksums, with the part count appended as `-N`. Comparing that to a CRC32
/// of the file always fails — the same trap as the multipart ETag, wearing a
/// different hat. Verifying one needs the exact part boundaries used at upload
/// time, which a download does not know.
fn is_composite(checksum: &str) -> bool {
    checksum
        .rsplit_once('-')
        .is_some_and(|(_, count)| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()))
}

/// Compares a downloaded file against what the server said.
pub fn verify(path: &Path, expected: Option<&str>) -> Result<Verification> {
    let Some(expected) = expected else {
        return Ok(Verification::Unavailable);
    };
    if is_composite(expected) {
        // Not a failure: the object is fine, this check simply cannot speak to
        // it. Reporting a mismatch here would fail every large download.
        return Ok(Verification::Unavailable);
    }
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
        assert_eq!(crc32_base64_of(0xCBF4_3926), "y/Q5Jg==");
    }

    #[test]
    fn base64_pads_a_four_byte_value_correctly() {
        assert_eq!(crc32_base64_of(0), "AAAAAA==");
        assert_eq!(crc32_base64_of(0xFFFF_FFFF), "/////w==");
        // Two padding characters always, because four bytes is never a whole
        // number of 3-byte groups.
        assert!(crc32_base64_of(0x1234_5678).ends_with("=="));
        assert_eq!(crc32_base64_of(0x1234_5678).len(), 8);
    }

    /// The encoder takes bytes; these tests think in CRC values.
    fn crc32_base64_of(value: u32) -> String {
        s3core::encode_crc32(value)
    }

    /// `tag` makes the name unique per test. Naming by content length collided
    /// between two tests using the same fixture, and since tests run in
    /// parallel one deleted the file the other was still reading.
    fn temp_file(tag: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("s3b-checksum-{}-{tag}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_composite_checksum_is_not_compared_against_the_whole_file() {
        // The trap: a multipart object's checksum is built from its parts, so a
        // whole-file CRC32 never equals it. Treating that as corruption would
        // fail every large download — exactly the case checksums exist for.
        assert!(is_composite("y/Q5Jg==-3"));
        assert!(is_composite("AAAAAA==-12"));

        // A single-part checksum has no suffix. Base64 can end in `=` or
        // contain `+` and `/`, none of which must be read as a part count.
        assert!(!is_composite("y/Q5Jg=="));
        assert!(!is_composite("/////w=="));
        // A trailing dash with no number is not a part count either.
        assert!(!is_composite("abc-"));

        let path = temp_file("composite", b"123456789");
        assert_eq!(
            verify(&path, Some("y/Q5Jg==-3")).unwrap(),
            Verification::Unavailable
        );
        _ = std::fs::remove_file(&path);
    }

    #[test]
    fn verify_distinguishes_matching_corrupt_and_unknown() {
        let path = temp_file("verify", b"123456789");
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
        let path = temp_file("empty", b"");
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
        let path = temp_file("big", &big);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&big);
        assert_eq!(
            crc32_of_file(&path).unwrap(),
            crc32_base64_of(hasher.finalize())
        );

        _ = std::fs::remove_file(&path);
    }
}
