//! Thin, UI-agnostic wrapper over `aws-sdk-s3`.
//!
//! Everything here is plain async Rust with no GPUI types, so it can be unit
//! tested without a window and reused by a future CLI.

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::types::{CompletedMultipartUpload, Delete, ObjectIdentifier};
use aws_sdk_s3::Client;

/// S3 refuses more than this many keys in one DeleteObjects call.
const DELETE_BATCH: usize = 1000;

/// One saved connection. Mirrors Cyberduck's "connection profile" idea: an
/// endpoint plus the quirk flags that provider needs.
#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    /// `None` for real AWS; set for MinIO, R2, B2, Wasabi, Spaces…
    pub endpoint: Option<String>,
    pub region: String,
    /// MinIO and most self-hosted stores need path-style addressing.
    pub path_style: bool,
    pub access_key: String,
    pub secret_key: String,
    /// aws-sdk-s3 >= 1.69 sends CRC32 checksum headers by default, which several
    /// S3-compatible providers reject. Relaxing them to "when required" is the
    /// documented workaround, so it defaults on for anything that isn't AWS.
    pub relaxed_checksums: bool,
}

impl Profile {
    /// Local MinIO with the stock development credentials.
    pub fn minio_local() -> Self {
        Self {
            name: "MinIO (local)".into(),
            endpoint: Some("http://127.0.0.1:9000".into()),
            region: "us-east-1".into(),
            path_style: true,
            access_key: "minioadmin".into(),
            secret_key: "minioadmin".into(),
            relaxed_checksums: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Name shown in the list, without the parent prefix.
    pub name: String,
    /// Full S3 key (for a folder, the prefix including its trailing slash).
    pub key: String,
    pub is_folder: bool,
    pub size: i64,
    /// Seconds since the epoch; S3 has no mtime for prefixes, hence the option.
    pub modified_epoch: Option<i64>,
    pub storage_class: Option<String>,
}

/// One page of a listing. S3 caps a page at 1000 keys, so the UI keeps
/// requesting while `continuation` is `Some`.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub continuation: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

impl Sort {
    /// Toggles direction when the same column is clicked again, otherwise
    /// switches column and starts ascending.
    pub fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                ascending: !self.ascending,
            }
        } else {
            Self {
                key,
                ascending: true,
            }
        }
    }
}

/// Sorts in place, always keeping folders above files the way Finder does.
pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    entries.sort_by(|a, b| {
        a.is_folder.cmp(&b.is_folder).reverse().then_with(|| {
            let ordering = match sort.key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified_epoch.cmp(&b.modified_epoch),
            };
            if sort.ascending {
                ordering
            } else {
                ordering.reverse()
            }
        })
    });
}

/// What `HeadObject` tells us before a download starts.
#[derive(Clone, Debug)]
pub struct ObjectHead {
    pub size: i64,
    pub etag: Option<String>,
}

/// One finished part of a multipart upload. Persisted so a resumed upload can
/// complete without re-sending what the server already has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedPart {
    pub part_number: i32,
    pub etag: String,
    pub size: u64,
}

/// A multipart upload left behind by a crash or a cancel. S3 keeps billing for
/// its parts until it is aborted.
#[derive(Clone, Debug)]
pub struct OrphanedUpload {
    pub key: String,
    pub upload_id: String,
    pub initiated_epoch: Option<i64>,
}

/// Result of a batch delete, so the UI can report partial failures honestly
/// rather than claiming success.
#[derive(Clone, Debug, Default)]
pub struct DeleteReport {
    pub deleted: usize,
    pub errors: Vec<String>,
}

#[derive(Clone)]
pub struct S3Client {
    inner: Client,
}

impl S3Client {
    pub async fn connect(profile: &Profile) -> Result<Self> {
        let creds = Credentials::new(
            profile.access_key.clone(),
            profile.secret_key.clone(),
            None,
            None,
            "s3browser-profile",
        );

        let sdk_config = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(profile.region.clone()))
            .credentials_provider(creds)
            .load()
            .await;

        let mut builder =
            aws_sdk_s3::config::Builder::from(&sdk_config).force_path_style(profile.path_style);

        if let Some(endpoint) = &profile.endpoint {
            builder = builder.endpoint_url(endpoint);
        }

        if profile.relaxed_checksums {
            builder = builder
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        }

        Ok(Self {
            inner: Client::from_conf(builder.build()),
        })
    }

    pub async fn list_buckets(&self) -> Result<Vec<String>> {
        let out = self
            .inner
            .list_buckets()
            .send()
            .await
            .context("ListBuckets failed")?;

        Ok(out
            .buckets()
            .iter()
            .filter_map(|b| b.name().map(str::to_owned))
            .collect())
    }

    /// Lists one page of a "folder": `prefix` plus `/` as delimiter turns the flat
    /// keyspace into the directory view users expect.
    pub async fn list_page(
        &self,
        bucket: &str,
        prefix: &str,
        continuation: Option<String>,
    ) -> Result<Page> {
        let mut req = self
            .inner
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .delimiter("/")
            .max_keys(1000);

        if let Some(token) = continuation {
            req = req.continuation_token(token);
        }

        let out = req
            .send()
            .await
            .with_context(|| format!("ListObjectsV2 failed for s3://{bucket}/{prefix}"))?;

        let mut entries = Vec::new();

        for cp in out.common_prefixes() {
            let Some(full) = cp.prefix() else { continue };
            let name = full
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(full)
                .to_string();
            entries.push(Entry {
                name,
                key: full.to_string(),
                is_folder: true,
                size: 0,
                modified_epoch: None,
                storage_class: None,
            });
        }

        for obj in out.contents() {
            let Some(key) = obj.key() else { continue };
            // A zero-byte `prefix/` object is the folder placeholder convention;
            // it is already represented by the common prefix above.
            if key == prefix || key.ends_with('/') {
                continue;
            }
            entries.push(Entry {
                name: key.rsplit('/').next().unwrap_or(key).to_string(),
                key: key.to_string(),
                is_folder: false,
                size: obj.size().unwrap_or(0),
                modified_epoch: obj.last_modified().map(|t| t.secs()),
                storage_class: obj.storage_class().map(|s| s.as_str().to_string()),
            });
        }

        Ok(Page {
            entries,
            continuation: out.next_continuation_token().map(str::to_owned),
        })
    }

    pub async fn create_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .with_context(|| format!("CreateBucket failed for {bucket}"))?;
        Ok(())
    }

    /// S3 only deletes empty buckets; the UI is expected to offer emptying first.
    pub async fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .with_context(|| format!("DeleteBucket failed for {bucket}"))?;
        Ok(())
    }

    /// Creates the zero-byte `prefix/` placeholder every S3 client uses to stand
    /// in for an empty folder.
    pub async fn create_folder(&self, bucket: &str, prefix: &str, name: &str) -> Result<String> {
        let key = format!("{prefix}{}/", name.trim_matches('/'));
        self.inner
            .put_object()
            .bucket(bucket)
            .key(&key)
            .body(Vec::new().into())
            .send()
            .await
            .with_context(|| format!("PutObject failed for s3://{bucket}/{key}"))?;
        Ok(key)
    }

    /// Deletes objects in batches of 1000. Folders are expanded to every key
    /// beneath them first, since S3 has no recursive delete.
    pub async fn delete_entries(&self, bucket: &str, entries: &[Entry]) -> Result<DeleteReport> {
        let mut keys = Vec::new();
        for entry in entries {
            if entry.is_folder {
                keys.extend(self.list_keys_recursive(bucket, &entry.key).await?);
                // The placeholder object itself, if one exists.
                keys.push(entry.key.clone());
            } else {
                keys.push(entry.key.clone());
            }
        }
        keys.sort();
        keys.dedup();

        let mut report = DeleteReport::default();
        for chunk in keys.chunks(DELETE_BATCH) {
            let mut delete = Delete::builder();
            for key in chunk {
                delete = delete.objects(ObjectIdentifier::builder().key(key).build()?);
            }

            let out = self
                .inner
                .delete_objects()
                .bucket(bucket)
                .delete(delete.build()?)
                .send()
                .await
                .with_context(|| format!("DeleteObjects failed for {bucket}"))?;

            report.deleted += out.deleted().len();
            for error in out.errors() {
                report.errors.push(format!(
                    "{}: {}",
                    error.key().unwrap_or("?"),
                    error.message().unwrap_or("unknown error")
                ));
            }
        }
        Ok(report)
    }

    // ------------------------------------------------------------- transfers

    /// Size and ETag, used to size a download and to detect that an object
    /// changed underneath a resumed transfer.
    pub async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectHead> {
        let out = self
            .inner
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("HeadObject failed for s3://{bucket}/{key}"))?;

        Ok(ObjectHead {
            size: out.content_length().unwrap_or(0),
            etag: out.e_tag().map(str::to_owned),
        })
    }

    /// Single-request upload, for objects below the multipart threshold.
    pub async fn put_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<()> {
        self.inner
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body.into())
            .send()
            .await
            .with_context(|| format!("PutObject failed for s3://{bucket}/{key}"))?;
        Ok(())
    }

    /// Reads a byte range. `If-Match` makes the server reject the read if the
    /// object changed since the transfer started, so a resumed download can
    /// never silently stitch together two different versions.
    pub async fn get_range(
        &self,
        bucket: &str,
        key: &str,
        range: std::ops::Range<u64>,
        if_match: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut req = self
            .inner
            .get_object()
            .bucket(bucket)
            .key(key)
            // HTTP ranges are inclusive at both ends.
            .range(format!("bytes={}-{}", range.start, range.end.saturating_sub(1)));

        if let Some(etag) = if_match {
            req = req.if_match(etag);
        }

        let out = req
            .send()
            .await
            .with_context(|| format!("GetObject range failed for s3://{bucket}/{key}"))?;

        let bytes = out
            .body
            .collect()
            .await
            .with_context(|| format!("reading body of s3://{bucket}/{key}"))?;
        Ok(bytes.into_bytes().to_vec())
    }

    pub async fn create_multipart_upload(&self, bucket: &str, key: &str) -> Result<String> {
        let out = self
            .inner
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("CreateMultipartUpload failed for s3://{bucket}/{key}"))?;

        out.upload_id()
            .map(str::to_owned)
            .context("CreateMultipartUpload returned no upload id")
    }

    pub async fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: i32,
        body: Vec<u8>,
    ) -> Result<String> {
        let out = self
            .inner
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(body.into())
            .send()
            .await
            .with_context(|| format!("UploadPart {part_number} failed for s3://{bucket}/{key}"))?;

        out.e_tag()
            .map(str::to_owned)
            .context("UploadPart returned no ETag")
    }

    /// Parts must be listed in ascending part-number order or S3 rejects the call.
    pub async fn complete_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        mut parts: Vec<CompletedPart>,
    ) -> Result<()> {
        parts.sort_by_key(|part| part.part_number);

        let mut completed = CompletedMultipartUpload::builder();
        for part in parts {
            completed = completed.parts(
                aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(part.part_number)
                    .e_tag(part.etag)
                    .build(),
            );
        }

        self.inner
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(completed.build())
            .send()
            .await
            .with_context(|| format!("CompleteMultipartUpload failed for s3://{bucket}/{key}"))?;
        Ok(())
    }

    /// Releases the storage S3 is already billing for an abandoned upload.
    pub async fn abort_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<()> {
        self.inner
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .with_context(|| format!("AbortMultipartUpload failed for s3://{bucket}/{key}"))?;
        Ok(())
    }

    /// Which parts the server already holds, so a resumed upload re-sends only
    /// what is missing rather than starting over.
    pub async fn list_parts(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<Vec<CompletedPart>> {
        let mut parts = Vec::new();
        let mut marker = None;

        loop {
            let mut req = self
                .inner
                .list_parts()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id);
            if let Some(marker) = marker {
                req = req.part_number_marker(marker);
            }

            let out = req
                .send()
                .await
                .with_context(|| format!("ListParts failed for s3://{bucket}/{key}"))?;

            for part in out.parts() {
                let (Some(number), Some(etag)) = (part.part_number(), part.e_tag()) else {
                    continue;
                };
                parts.push(CompletedPart {
                    part_number: number,
                    etag: etag.to_string(),
                    size: part.size().unwrap_or(0) as u64,
                });
            }

            if out.is_truncated().unwrap_or(false) {
                marker = out.next_part_number_marker().map(str::to_owned);
                if marker.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(parts)
    }

    /// Uploads that were started and never finished or aborted. S3 bills for
    /// their parts indefinitely, and they are invisible in a normal listing.
    pub async fn list_orphaned_uploads(&self, bucket: &str) -> Result<Vec<OrphanedUpload>> {
        let out = self
            .inner
            .list_multipart_uploads()
            .bucket(bucket)
            .send()
            .await
            .with_context(|| format!("ListMultipartUploads failed for {bucket}"))?;

        Ok(out
            .uploads()
            .iter()
            .filter_map(|upload| {
                Some(OrphanedUpload {
                    key: upload.key()?.to_string(),
                    upload_id: upload.upload_id()?.to_string(),
                    initiated_epoch: upload.initiated().map(|t| t.secs()),
                })
            })
            .collect())
    }

    /// Every key under `prefix`, paging until exhausted. No delimiter here — we
    /// want the whole subtree flat.
    async fn list_keys_recursive(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token = None;

        loop {
            let mut req = self
                .inner
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .max_keys(1000);
            if let Some(token) = token {
                req = req.continuation_token(token);
            }

            let out = req
                .send()
                .await
                .with_context(|| format!("ListObjectsV2 failed for s3://{bucket}/{prefix}"))?;

            keys.extend(out.contents().iter().filter_map(|o| o.key().map(str::to_owned)));

            match out.next_continuation_token() {
                Some(next) => token = Some(next.to_string()),
                None => break,
            }
        }
        Ok(keys)
    }
}

/// Human-readable byte size, e.g. `4.2 MB`.
pub fn format_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `2026-08-18 15:16` in local-independent UTC, which is what S3 reports.
pub fn format_timestamp(epoch: i64) -> String {
    // Civil-from-days, so we avoid pulling in a date library for one label.
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_folder: bool, size: i64, modified: Option<i64>) -> Entry {
        Entry {
            name: name.into(),
            key: name.into(),
            is_folder,
            size,
            modified_epoch: modified,
            storage_class: None,
        }
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00");
        assert_eq!(format_timestamp(1_787_073_380), "2026-08-18 17:16");
        // Leap day, and the last second before a year rolls over.
        assert_eq!(format_timestamp(1_709_208_000), "2024-02-29 12:00");
        assert_eq!(format_timestamp(1_767_225_599), "2025-12-31 23:59");
    }

    #[test]
    fn sorts_folders_first_regardless_of_column() {
        let mut entries = vec![
            entry("zeta.txt", false, 10, Some(100)),
            entry("alpha", true, 0, None),
        ];
        for key in [SortKey::Name, SortKey::Size, SortKey::Modified] {
            for ascending in [true, false] {
                sort_entries(&mut entries, Sort { key, ascending });
                assert!(
                    entries[0].is_folder,
                    "folder should lead for {key:?} ascending={ascending}"
                );
            }
        }
    }

    #[test]
    fn sort_toggles_direction_only_on_the_same_column() {
        let sort = Sort::default();
        assert!(sort.ascending);

        let same = sort.toggled(SortKey::Name);
        assert!(!same.ascending, "same column flips direction");

        let switched = same.toggled(SortKey::Size);
        assert_eq!(switched.key, SortKey::Size);
        assert!(switched.ascending, "new column starts ascending");
    }

    #[test]
    fn sorts_by_size_then_name() {
        let mut entries = vec![
            entry("big.bin", false, 900, None),
            entry("small.txt", false, 10, None),
        ];
        sort_entries(
            &mut entries,
            Sort {
                key: SortKey::Size,
                ascending: true,
            },
        );
        assert_eq!(entries[0].name, "small.txt");
    }
}
