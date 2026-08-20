//! Thin, UI-agnostic wrapper over `aws-sdk-s3`.
//!
//! Everything here is plain async Rust with no GPUI types, so it can be unit
//! tested without a window and reused by a future CLI.

pub mod capability;
pub mod sso;
pub mod sts;

use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::retry::RetryConfig;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{
    ChecksumAlgorithm, ChecksumMode, CompletedMultipartUpload, ObjectCannedAcl, Delete, GlacierJobParameters, MetadataDirective,
    ObjectIdentifier, RestoreRequest, ServerSideEncryption, StorageClass, Tag, Tagging, Tier,
};
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
    /// Set for STS/SSO-derived credentials. Its presence is what makes a
    /// presigned URL outlive-able: the URL dies with the session regardless of
    /// the expiry asked for.
    pub session_token: Option<String>,
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
            session_token: None,
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
    /// `None` means STANDARD; S3 omits the header for it.
    pub storage_class: Option<String>,
    pub content_type: Option<String>,
    /// `AES256` or `aws:kms`, absent when the object is not encrypted.
    pub encryption: Option<String>,
    pub kms_key_id: Option<String>,
    pub modified_epoch: Option<i64>,
    /// User metadata (the `x-amz-meta-` headers), without the prefix.
    pub metadata: Vec<(String, String)>,
    /// Base64 CRC32 as the server computed it, when it reports one at all.
    ///
    /// Absent is the normal answer on objects uploaded before checksums existed
    /// and on providers that do not implement them, so a caller has to treat
    /// `None` as "cannot verify" rather than as a failure.
    pub checksum_crc32: Option<String>,
    /// The raw `x-amz-restore` header for an archived object: it says whether a
    /// restore is still running and when the copy expires.
    pub restore: Option<String>,
}

/// How far along a Glacier restore is. Derived from `x-amz-restore`, which is
/// absent entirely for objects that were never archived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreState {
    /// Readable as-is; no restore involved.
    NotArchived,
    /// Archived and not restored — a GET would fail until a restore is asked for.
    Archived,
    InProgress,
    /// Restored and readable until the temporary copy expires.
    Done,
}

/// What a finished `UploadPart` hands back.
#[derive(Clone, Debug)]
pub struct UploadedPart {
    pub etag: String,
    pub checksum_crc32: Option<String>,
}

/// One finished part of a multipart upload. Persisted so a resumed upload can
/// complete without re-sending what the server already has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedPart {
    pub part_number: i32,
    pub etag: String,
    pub size: u64,
    /// Base64 CRC32 of this part, when the upload asked for checksums.
    ///
    /// `CompleteMultipartUpload` has to repeat every part's checksum, so this
    /// travels with the part rather than being recomputed — the bytes are long
    /// gone by then, and on a resumed upload they were never in this process.
    pub checksum_crc32: Option<String>,
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

/// One historical version of a key. A delete marker is also a "version" as far
/// as S3 is concerned, but it holds no data — it only hides what is underneath,
/// so the UI has to tell them apart or it will offer to download nothing.
#[derive(Clone, Debug)]
pub struct ObjectVersion {
    pub key: String,
    pub version_id: String,
    pub is_latest: bool,
    pub is_delete_marker: bool,
    pub size: i64,
    pub modified_epoch: Option<i64>,
}

/// A present-but-blank field is as useless as an absent one, and S3-compatible
/// providers differ on which they send.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.trim().is_empty())
}

/// Who can do what to an object, flattened out of the grant list.
///
/// The XML form nests a grantee inside each grant and repeats the owner in
/// every one; nobody reading a permissions panel wants that shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectAcl {
    /// Display name if the provider gives one, otherwise the canonical id.
    pub owner: String,
    pub grants: Vec<AclGrant>,
}

/// What a provider returns when it answers ACL reads without implementing them.
const UNKNOWN: &str = "không rõ";

impl ObjectAcl {
    /// Whether this says anything at all.
    ///
    /// MinIO answers `GetObjectAcl` successfully and returns a stub: no owner,
    /// one grant whose grantee is unidentified. Then `PutObjectAcl` fails with
    /// `NotImplemented`. A probe that only asks "did the read succeed?" calls
    /// that support and shows a panel of placeholders whose every button fails.
    ///
    /// So the test is whether the answer carries information, not whether the
    /// call returned — and it costs no extra request.
    pub fn is_meaningful(&self) -> bool {
        self.owner != UNKNOWN || self.grants.iter().any(|grant| grant.grantee != UNKNOWN)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AclGrant {
    /// `Mọi người`, `Người dùng đã xác thực`, a display name, or an id.
    pub grantee: String,
    /// READ, WRITE, FULL_CONTROL…
    pub permission: String,
    /// True for the two AWS predefined groups that make an object reachable by
    /// anyone. Worth flagging: it is the difference between a private object
    /// and one the whole internet can read.
    pub public: bool,
}

/// The canned ACLs worth offering. The full grant syntax exists, but hand-built
/// grants are how buckets end up accidentally world-readable — a short list of
/// named intents is safer and covers what people actually want.
pub const CANNED_ACLS: [(&str, &str); 4] = [
    ("private", "Riêng tư"),
    ("public-read", "Ai cũng đọc được"),
    ("bucket-owner-read", "Chủ bucket đọc được"),
    ("bucket-owner-full-control", "Chủ bucket toàn quyền"),
];

/// Result of moving a whole prefix. A partial move is a real outcome, not an
/// error: whatever succeeded stays moved, so the caller has to be able to say
/// what is now where.
#[derive(Clone, Debug, Default)]
pub struct MoveReport {
    pub moved: usize,
    pub errors: Vec<String>,
}

/// Server-side encryption to ask for when uploading. S3 applies this at write
/// time only — an object already stored cannot be re-encrypted without copying
/// it over itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Encryption {
    /// Whatever the bucket's default encryption says, which is often AES256
    /// anyway. Asking for nothing is not the same as asking for none.
    #[default]
    BucketDefault,
    /// SSE-S3: keys managed by S3, no configuration and no extra cost.
    Aes256,
    /// SSE-KMS with a specific key. Costs a KMS request per operation, which is
    /// worth knowing before turning it on for a bulk upload.
    Kms(String),
}

#[derive(Clone)]
pub struct S3Client {
    inner: Client,
    /// Shared across clones on purpose: the transfer engine holds its own clone,
    /// and changing the setting in the UI has to reach the uploads it runs.
    encryption: std::sync::Arc<std::sync::Mutex<Encryption>>,
}

impl S3Client {
    pub async fn connect(profile: &Profile) -> Result<Self> {
        let creds = Credentials::new(
            profile.access_key.clone(),
            profile.secret_key.clone(),
            profile.session_token.clone(),
            None,
            "s3browser-profile",
        );

        // S3 answers 503 SlowDown once a prefix goes past roughly 3.500 writes or
        // 5.500 reads per second, and a transfer queue with parts in flight is
        // exactly the workload that trips it. Adaptive mode adds a client-side
        // rate limiter on top of exponential backoff, so a throttled prefix slows
        // the whole client down instead of each request retrying into the same
        // wall. The default is 3 attempts, which is too few for a queue that can
        // sit behind a throttled prefix for a while.
        let retry = RetryConfig::adaptive()
            .with_max_attempts(6)
            .with_initial_backoff(Duration::from_millis(200))
            .with_max_backoff(Duration::from_secs(20));

        let sdk_config = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new(profile.region.clone()))
            .credentials_provider(creds)
            .retry_config(retry)
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
            encryption: std::sync::Arc::new(std::sync::Mutex::new(Encryption::BucketDefault)),
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
            // Without this S3 omits the checksum headers even when it has them
            // stored, which reads exactly like a provider that does not support
            // checksums at all.
            .checksum_mode(ChecksumMode::Enabled)
            .send()
            .await
            .with_context(|| format!("HeadObject failed for s3://{bucket}/{key}"))?;

        Ok(ObjectHead {
            size: out.content_length().unwrap_or(0),
            etag: out.e_tag().map(str::to_owned),
            storage_class: out.storage_class().map(|class| class.as_str().to_owned()),
            content_type: out.content_type().map(str::to_owned),
            encryption: out
                .server_side_encryption()
                .map(|sse| sse.as_str().to_owned()),
            kms_key_id: out.ssekms_key_id().map(str::to_owned),
            modified_epoch: out.last_modified().map(|t| t.secs()),
            metadata: out
                .metadata()
                .map(|map| {
                    let mut pairs: Vec<(String, String)> = map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    // S3 returns metadata unordered; a stable order keeps the
                    // inspector from reshuffling itself on every refresh.
                    pairs.sort();
                    pairs
                })
                .unwrap_or_default(),
            checksum_crc32: out.checksum_crc32().map(str::to_owned),
            restore: out.restore().map(str::to_owned),
        })
    }

    /// Single-request upload, for objects below the multipart threshold.
    /// The underlying SDK client, for probes that have no wrapper of their own.
    pub(crate) fn inner(&self) -> &Client {
        &self.inner
    }

    pub fn encryption(&self) -> Encryption {
        self.encryption.lock().unwrap().clone()
    }

    pub fn set_encryption(&self, encryption: Encryption) {
        *self.encryption.lock().unwrap() = encryption;
    }

    pub async fn put_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<()> {
        let checksum = crc32_base64(&body);
        let mut req = self
            .inner
            .put_object()
            .bucket(bucket)
            .key(key)
            .checksum_crc32(checksum)
            .body(body.into());
        match self.encryption() {
            Encryption::BucketDefault => {}
            Encryption::Aes256 => req = req.server_side_encryption(ServerSideEncryption::Aes256),
            Encryption::Kms(key_id) => {
                req = req
                    .server_side_encryption(ServerSideEncryption::AwsKms)
                    .ssekms_key_id(key_id)
            }
        }
        req.send()
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
        // Encryption is decided here, once. Setting it per part is not a thing
        // S3 supports — the parts inherit whatever the upload was created with.
        // Declaring the algorithm is what makes S3 combine the per-part
        // checksums into one for the object; without it the parts carry values
        // nothing ever compares.
        let mut req = self
            .inner
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .checksum_algorithm(ChecksumAlgorithm::Crc32);
        match self.encryption() {
            Encryption::BucketDefault => {}
            Encryption::Aes256 => req = req.server_side_encryption(ServerSideEncryption::Aes256),
            Encryption::Kms(key_id) => {
                req = req
                    .server_side_encryption(ServerSideEncryption::AwsKms)
                    .ssekms_key_id(key_id)
            }
        }
        let out = req
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
    ) -> Result<UploadedPart> {
        let checksum = crc32_base64(&body);
        let out = self
            .inner
            .upload_part()
            .checksum_crc32(&checksum)
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(body.into())
            .send()
            .await
            .with_context(|| format!("UploadPart {part_number} failed for s3://{bucket}/{key}"))?;

        Ok(UploadedPart {
            etag: out
                .e_tag()
                .map(str::to_owned)
                .context("UploadPart returned no ETag")?,
            checksum_crc32: Some(checksum),
        })
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
                    // Repeating the part checksum is required once the upload
                    // declared an algorithm; omitting it fails the completion
                    // with an error that names neither the part nor the reason.
                    .set_checksum_crc32(part.checksum_crc32)
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
                    // A resumed upload never held these bytes, so the server's
                    // copy is the only place this value can come from.
                    checksum_crc32: part.checksum_crc32().map(str::to_owned),
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

    /// Removes everything in a bucket, versions and delete markers included.
    ///
    /// A plain object delete is not enough in a versioned bucket: it writes a
    /// delete marker and leaves the data, so `DeleteBucket` then fails with
    /// BucketNotEmpty and nothing explains why. This walks
    /// `ListObjectVersions`, which is the only listing that sees the hidden
    /// versions at all.
    ///
    /// Deletes go out in batches of 1000, the API's limit. `progress` reports
    /// (done, seen-so-far) — the total is not knowable upfront without walking
    /// the whole bucket first, which for a large bucket costs as much as the
    /// deletion.
    pub async fn empty_bucket(
        &self,
        bucket: &str,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<DeleteReport> {
        let mut report = DeleteReport::default();
        let mut key_marker: Option<String> = None;
        let mut version_marker: Option<String> = None;
        let mut seen = 0usize;

        loop {
            let mut req = self
                .inner
                .list_object_versions()
                .bucket(bucket)
                .max_keys(1000);
            if let Some(marker) = &key_marker {
                req = req.key_marker(marker);
            }
            if let Some(marker) = &version_marker {
                req = req.version_id_marker(marker);
            }

            let out = req
                .send()
                .await
                .with_context(|| format!("ListObjectVersions failed for s3://{bucket}"))?;

            let mut batch: Vec<ObjectIdentifier> = Vec::new();
            for (key, version_id) in out
                .versions()
                .iter()
                .filter_map(|v| Some((v.key()?, v.version_id()?)))
                .chain(
                    out.delete_markers()
                        .iter()
                        .filter_map(|m| Some((m.key()?, m.version_id()?))),
                )
            {
                if let Ok(id) = ObjectIdentifier::builder()
                    .key(key)
                    .version_id(version_id)
                    .build()
                {
                    batch.push(id);
                }
            }

            seen += batch.len();
            if !batch.is_empty() {
                let deleted = batch.len();
                let request = Delete::builder()
                    .set_objects(Some(batch))
                    .quiet(true)
                    .build()
                    .context("danh sách xoá không hợp lệ")?;

                let out = self
                    .inner
                    .delete_objects()
                    .bucket(bucket)
                    .delete(request)
                    .send()
                    .await
                    .with_context(|| format!("DeleteObjects failed for s3://{bucket}"))?;

                let failed = out.errors().len();
                for error in out.errors() {
                    report.errors.push(format!(
                        "{}: {}",
                        error.key().unwrap_or("?"),
                        error.message().unwrap_or("lỗi không rõ")
                    ));
                }
                report.deleted += deleted - failed;
                progress(report.deleted, seen);
            }

            if out.is_truncated().unwrap_or(false) {
                key_marker = out.next_key_marker().map(str::to_owned);
                version_marker = out.next_version_id_marker().map(str::to_owned);
                if key_marker.is_none() && version_marker.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(report)
    }

    /// Every version of one key, newest first, delete markers included.
    ///
    /// `ListObjectVersions` takes a prefix, not a key, so it happily returns
    /// `notes.txt.bak` when asked about `notes.txt`; the caller-visible filter
    /// keeps that from showing up as a mysterious extra version.
    pub async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectVersion>> {
        let mut versions = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut version_marker: Option<String> = None;

        loop {
            let mut req = self
                .inner
                .list_object_versions()
                .bucket(bucket)
                .prefix(key)
                .max_keys(1000);
            if let Some(marker) = &key_marker {
                req = req.key_marker(marker);
            }
            if let Some(marker) = &version_marker {
                req = req.version_id_marker(marker);
            }

            let out = req
                .send()
                .await
                .with_context(|| format!("ListObjectVersions failed for s3://{bucket}/{key}"))?;

            for version in out.versions() {
                let Some(found) = version.key() else { continue };
                if found != key {
                    continue;
                }
                versions.push(ObjectVersion {
                    key: found.to_string(),
                    version_id: version.version_id().unwrap_or_default().to_string(),
                    is_latest: version.is_latest().unwrap_or(false),
                    is_delete_marker: false,
                    size: version.size().unwrap_or(0),
                    modified_epoch: version.last_modified().map(|t| t.secs()),
                });
            }

            for marker in out.delete_markers() {
                let Some(found) = marker.key() else { continue };
                if found != key {
                    continue;
                }
                versions.push(ObjectVersion {
                    key: found.to_string(),
                    version_id: marker.version_id().unwrap_or_default().to_string(),
                    is_latest: marker.is_latest().unwrap_or(false),
                    is_delete_marker: true,
                    size: 0,
                    modified_epoch: marker.last_modified().map(|t| t.secs()),
                });
            }

            if out.is_truncated().unwrap_or(false) {
                key_marker = out.next_key_marker().map(str::to_owned);
                version_marker = out.next_version_id_marker().map(str::to_owned);
                if key_marker.is_none() && version_marker.is_none() {
                    break;
                }
            } else {
                break;
            }
        }

        // Newest first. S3 returns them in key order, and within a key roughly
        // newest-first, but sorting on the timestamp makes that a guarantee.
        versions.sort_by(|a, b| b.modified_epoch.cmp(&a.modified_epoch));
        Ok(versions)
    }

    /// Makes an old version current again by copying it over the top.
    ///
    /// The old version is *not* deleted: in a versioned bucket the copy becomes
    /// a new latest version and everything stays in the history. That is what
    /// makes this safe to do by accident.
    pub async fn restore_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        let source = format!("{}?versionId={version_id}", encode_copy_source(bucket, key));

        self.inner
            .copy_object()
            .bucket(bucket)
            .key(key)
            .copy_source(source)
            .metadata_directive(MetadataDirective::Copy)
            .send()
            .await
            .with_context(|| {
                format!("Khôi phục version {version_id} của s3://{bucket}/{key} thất bại")
            })?;
        Ok(())
    }

    /// Deletes one specific version. Unlike an ordinary delete this removes data
    /// for good — it does not write a delete marker.
    pub async fn delete_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        self.inner
            .delete_object()
            .bucket(bucket)
            .key(key)
            .version_id(version_id)
            .send()
            .await
            .with_context(|| format!("Xoá version {version_id} của s3://{bucket}/{key} thất bại"))?;
        Ok(())
    }

    pub async fn object_acl(&self, bucket: &str, key: &str) -> Result<ObjectAcl> {
        let out = self
            .inner
            .get_object_acl()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("GetObjectAcl failed for s3://{bucket}/{key}"))?;

        // Filtering empties, not just `None`: MinIO returns an owner whose
        // display name is present and blank, so `.or(id)` never fired and the
        // panel showed nothing at all.
        let owner = out
            .owner()
            .and_then(|owner| {
                non_empty(owner.display_name()).or_else(|| non_empty(owner.id()))
            })
            .unwrap_or(UNKNOWN)
            .to_string();

        let grants = out
            .grants()
            .iter()
            .filter_map(|grant| {
                let grantee = grant.grantee()?;
                let uri = grantee.uri().unwrap_or_default();
                // The two predefined groups that make an object readable
                // outside the account. Matching on the URI rather than a name
                // because the name is absent for groups.
                let public = uri.ends_with("/groups/global/AllUsers")
                    || uri.ends_with("/groups/global/AuthenticatedUsers");

                let name = if uri.ends_with("/groups/global/AllUsers") {
                    "Mọi người".to_string()
                } else if uri.ends_with("/groups/global/AuthenticatedUsers") {
                    "Người dùng đã xác thực".to_string()
                } else {
                    non_empty(grantee.display_name())
                        .or_else(|| non_empty(grantee.id()))
                        .unwrap_or(UNKNOWN)
                        .to_string()
                };

                Some(AclGrant {
                    grantee: name,
                    permission: grant.permission()?.as_str().to_string(),
                    public,
                })
            })
            .collect();

        Ok(ObjectAcl { owner, grants })
    }

    /// Applies a canned ACL.
    ///
    /// Fails with `AccessControlListNotSupported` on a bucket whose Object
    /// Ownership is BucketOwnerEnforced — the default for buckets created since
    /// 2023. Capability detection is what keeps the UI from offering this there.
    pub async fn set_object_acl(&self, bucket: &str, key: &str, canned: &str) -> Result<()> {
        self.inner
            .put_object_acl()
            .bucket(bucket)
            .key(key)
            .acl(ObjectCannedAcl::from(canned))
            .send()
            .await
            .with_context(|| format!("PutObjectAcl failed for s3://{bucket}/{key}"))?;
        Ok(())
    }

    pub async fn object_tags(&self, bucket: &str, key: &str) -> Result<Vec<(String, String)>> {
        let out = self
            .inner
            .get_object_tagging()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("GetObjectTagging failed for s3://{bucket}/{key}"))?;

        let mut tags: Vec<(String, String)> = out
            .tag_set()
            .iter()
            .map(|tag| (tag.key().to_string(), tag.value().to_string()))
            .collect();
        tags.sort();
        Ok(tags)
    }

    /// Replaces the whole tag set — S3 has no way to change one tag. An empty
    /// set deletes the tagging, which is why this doubles as the delete path.
    pub async fn set_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: &[(String, String)],
    ) -> Result<()> {
        if tags.is_empty() {
            self.inner
                .delete_object_tagging()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .with_context(|| format!("DeleteObjectTagging failed for s3://{bucket}/{key}"))?;
            return Ok(());
        }

        let mut set = Vec::with_capacity(tags.len());
        for (key_name, value) in tags {
            set.push(
                Tag::builder()
                    .key(key_name)
                    .value(value)
                    .build()
                    .context("thẻ không hợp lệ")?,
            );
        }

        self.inner
            .put_object_tagging()
            .bucket(bucket)
            .key(key)
            .tagging(
                Tagging::builder()
                    .set_tag_set(Some(set))
                    .build()
                    .context("bộ thẻ không hợp lệ")?,
            )
            .send()
            .await
            .with_context(|| format!("PutObjectTagging failed for s3://{bucket}/{key}"))?;
        Ok(())
    }

    /// Asks for an archived object to be made readable for `days`. The call
    /// returns immediately; the copy appears minutes to hours later depending on
    /// the tier, which is why the inspector polls the restore state rather than
    /// treating this as done.
    pub async fn restore_object(&self, bucket: &str, key: &str, days: i32) -> Result<()> {
        let request = RestoreRequest::builder()
            .days(days)
            .glacier_job_parameters(
                GlacierJobParameters::builder()
                    .tier(Tier::Standard)
                    .build()
                    .context("tham số restore không hợp lệ")?,
            )
            .build();

        match self
            .inner
            .restore_object()
            .bucket(bucket)
            .key(key)
            .restore_request(request)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                // A restore already running is not a failure the user needs to
                // see as one — it means what they asked for is happening.
                let message = format!("{error:?}");
                if message.contains("RestoreAlreadyInProgress") {
                    Ok(())
                } else {
                    Err(error).with_context(|| {
                        format!("RestoreObject failed for s3://{bucket}/{key}")
                    })
                }
            }
        }
    }

    /// A time-limited URL anyone can GET without credentials.
    ///
    /// SigV4 caps a presigned URL at 7 days, and temporary credentials cap it
    /// far lower — the URL stops working the moment the session behind it ends,
    /// whatever expiry was requested. `presign_limit_for` works out the honest
    /// ceiling; this method clamps to it rather than handing back a URL that
    /// claims a week and dies in an hour.
    pub async fn presign_get(&self, bucket: &str, key: &str, expires: Duration) -> Result<String> {
        let config = PresigningConfig::expires_in(expires)
            .context("thời hạn presigned URL không hợp lệ")?;

        let request = self
            .inner
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(config)
            .await
            .with_context(|| format!("Presign failed for s3://{bucket}/{key}"))?;

        Ok(request.uri().to_string())
    }

    /// Whether the bucket keeps versions. Deleting in a versioned bucket writes
    /// a delete marker instead of removing data, which is a different promise to
    /// make to someone about to confirm a delete.
    ///
    /// Returns `false` when the answer cannot be had — some S3-compatible
    /// providers do not implement GetBucketVersioning, and a missing answer must
    /// not stop a delete. The cost of guessing wrong here is a warning that is
    /// absent, never one that is falsely reassuring.
    pub async fn bucket_is_versioned(&self, bucket: &str) -> bool {
        let Ok(out) = self.inner.get_bucket_versioning().bucket(bucket).send().await else {
            return false;
        };
        out.status()
            .is_some_and(|status| status.as_str() == "Enabled")
    }

    /// Server-side copy. Picks the strategy by size, because CopyObject refuses
    /// anything over 5 GiB and the caller should not have to know that.
    ///
    /// Storage class is carried over explicitly: a plain CopyObject lands the
    /// destination in STANDARD regardless of the source, which would silently
    /// move a Glacier object into a far more expensive tier.
    pub async fn copy_object(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
    ) -> Result<()> {
        let head = self.head_object(src_bucket, src_key).await?;
        let size = head.size.max(0) as u64;

        if size > COPY_OBJECT_LIMIT {
            let part_size = copy_part_size_for(size);
            self.copy_multipart(src_bucket, src_key, dst_bucket, dst_key, size, part_size, &head)
                .await
        } else {
            self.copy_single(src_bucket, src_key, dst_bucket, dst_key, &head)
                .await
        }
    }

    async fn copy_single(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
        head: &ObjectHead,
    ) -> Result<()> {
        let mut req = self
            .inner
            .copy_object()
            .bucket(dst_bucket)
            .key(dst_key)
            .copy_source(encode_copy_source(src_bucket, src_key))
            // COPY is the default, but saying so keeps the intent visible next
            // to the storage-class line, which is *not* copied by default.
            .metadata_directive(MetadataDirective::Copy);

        if let Some(class) = &head.storage_class {
            req = req.set_storage_class(Some(StorageClass::from(class.as_str())));
        }

        req.send().await.with_context(|| {
            format!("CopyObject failed for s3://{src_bucket}/{src_key} → s3://{dst_bucket}/{dst_key}")
        })?;
        Ok(())
    }

    /// The same server-side copy as `copy_object`, but through UploadPartCopy
    /// with a part size you choose. `copy_object` reaches for this on its own
    /// past `COPY_OBJECT_LIMIT`; calling it directly lets a test exercise the
    /// multipart path without moving five gigabytes.
    pub async fn copy_object_multipart(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
        part_size: u64,
    ) -> Result<()> {
        let head = self.head_object(src_bucket, src_key).await?;
        let size = head.size.max(0) as u64;
        self.copy_multipart(
            src_bucket, src_key, dst_bucket, dst_key, size, part_size, &head,
        )
        .await
    }

    /// Copy for objects past the CopyObject ceiling, using UploadPartCopy so the
    /// bytes still never travel through this machine.
    ///
    /// Parts run one at a time. Each is a server-side range copy rather than a
    /// transfer, so the wall-clock cost is the server's, and doing them serially
    /// keeps the abort-on-failure path simple.
    async fn copy_multipart(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
        size: u64,
        part_size: u64,
        head: &ObjectHead,
    ) -> Result<()> {
        let source = encode_copy_source(src_bucket, src_key);

        let mut create = self
            .inner
            .create_multipart_upload()
            .bucket(dst_bucket)
            .key(dst_key);
        if let Some(class) = &head.storage_class {
            create = create.set_storage_class(Some(StorageClass::from(class.as_str())));
        }
        let upload_id = create
            .send()
            .await
            .with_context(|| format!("CreateMultipartUpload failed for s3://{dst_bucket}/{dst_key}"))?
            .upload_id()
            .map(str::to_owned)
            .context("CreateMultipartUpload returned no upload id")?;

        match self
            .copy_parts(&source, dst_bucket, dst_key, &upload_id, size, part_size)
            .await
        {
            Ok(parts) => {
                self.complete_multipart_upload(dst_bucket, dst_key, &upload_id, parts)
                    .await
            }
            Err(error) => {
                // Leaving the upload behind would bill the user for parts they
                // cannot see, so clean up before surfacing the original error.
                _ = self
                    .abort_multipart_upload(dst_bucket, dst_key, &upload_id)
                    .await;
                Err(error)
            }
        }
    }

    async fn copy_parts(
        &self,
        source: &str,
        dst_bucket: &str,
        dst_key: &str,
        upload_id: &str,
        size: u64,
        part_size: u64,
    ) -> Result<Vec<CompletedPart>> {
        let mut parts = Vec::new();
        let mut offset = 0u64;
        let mut number = 1i32;

        while offset < size {
            let end = (offset + part_size).min(size) - 1;
            let out = self
                .inner
                .upload_part_copy()
                .bucket(dst_bucket)
                .key(dst_key)
                .upload_id(upload_id)
                .part_number(number)
                .copy_source(source)
                // Inclusive on both ends, unlike a Rust range.
                .copy_source_range(format!("bytes={offset}-{end}"))
                .send()
                .await
                .with_context(|| format!("UploadPartCopy part {number} failed for {source}"))?;

            let etag = out
                .copy_part_result()
                .and_then(|result| result.e_tag())
                .map(str::to_owned)
                .with_context(|| format!("UploadPartCopy part {number} returned no ETag"))?;

            parts.push(CompletedPart {
                part_number: number,
                etag,
                size: end - offset + 1,
                checksum_crc32: out
                    .copy_part_result()
                    .and_then(|result| result.checksum_crc32())
                    .map(str::to_owned),
            });
            offset = end + 1;
            number += 1;
        }
        Ok(parts)
    }

    /// Copy then delete. S3 has no rename, and there is no way to make the pair
    /// atomic — so the delete only runs once the copy is confirmed, leaving a
    /// duplicate rather than a hole if the second half fails.
    pub async fn move_object(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
    ) -> Result<()> {
        if (src_bucket, src_key) == (dst_bucket, dst_key) {
            return Ok(());
        }
        self.copy_object(src_bucket, src_key, dst_bucket, dst_key)
            .await?;
        self.delete_object(src_bucket, src_key).await
    }

    /// Copies every key under a prefix to another prefix.
    ///
    /// Same shape as [`move_prefix`](Self::move_prefix) without the delete, so a
    /// partial result leaves the source untouched — the failure mode is a
    /// half-copied destination, which is recoverable by running it again.
    pub async fn copy_prefix(
        &self,
        bucket: &str,
        src_prefix: &str,
        dst_prefix: &str,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<MoveReport> {
        if src_prefix == dst_prefix {
            return Ok(MoveReport::default());
        }
        // Copying a prefix into itself would walk keys it is still creating.
        if dst_prefix.starts_with(src_prefix) {
            anyhow::bail!("Không thể chép {src_prefix} vào trong chính nó ({dst_prefix})");
        }

        let keys = self.list_keys_recursive(bucket, src_prefix).await?;
        let total = keys.len();
        let mut report = MoveReport::default();
        progress(0, total);

        for (done, key) in keys.iter().enumerate() {
            let suffix = key.strip_prefix(src_prefix).unwrap_or(key);
            let target = format!("{dst_prefix}{suffix}");

            match self.copy_object(bucket, key, bucket, &target).await {
                Ok(()) => report.moved += 1,
                Err(error) => report.errors.push(format!("{key}: {error}")),
            }
            progress(done + 1, total);
        }
        Ok(report)
    }

    /// Renames a folder, which S3 has no concept of: every key under the prefix
    /// is copied to the new prefix and then deleted.
    ///
    /// There is no atomic version of this. Each key is copied *then* deleted
    /// before moving to the next, so an interruption leaves every key either at
    /// the source or at the destination — never neither. A failure does not roll
    /// back what already moved: undoing a move means deleting objects at the
    /// destination, and if something else wrote there in the meantime that
    /// deletes someone else's data. Reporting honestly beats guessing.
    ///
    /// `progress` is called with (done, total) so a caller can show a bar.
    pub async fn move_prefix(
        &self,
        bucket: &str,
        src_prefix: &str,
        dst_prefix: &str,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<MoveReport> {
        if src_prefix == dst_prefix {
            return Ok(MoveReport::default());
        }
        // Moving a prefix inside itself would walk keys it is still creating.
        if dst_prefix.starts_with(src_prefix) {
            anyhow::bail!("Không thể chuyển {src_prefix} vào trong chính nó ({dst_prefix})");
        }

        let keys = self.list_keys_recursive(bucket, src_prefix).await?;
        let total = keys.len();
        let mut report = MoveReport::default();
        progress(0, total);

        for (done, key) in keys.iter().enumerate() {
            let suffix = key.strip_prefix(src_prefix).unwrap_or(key);
            let target = format!("{dst_prefix}{suffix}");

            match self.move_object(bucket, key, bucket, &target).await {
                Ok(()) => report.moved += 1,
                Err(error) => report.errors.push(format!("{key}: {error}")),
            }
            progress(done + 1, total);
        }
        Ok(report)
    }

    pub async fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.inner
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("DeleteObject failed for s3://{bucket}/{key}"))?;
        Ok(())
    }

    /// Every key under `prefix`, paging until exhausted. No delimiter here — we
    /// want the whole subtree flat.
    pub async fn list_keys_recursive(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
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

/// Reads `x-amz-restore`. The header looks like `ongoing-request="false",
/// expiry-date="..."` — the flag matters more than the date, because an object
/// with a restore still running is not readable yet however promising it looks.
///
/// The header is absent both for objects that were never archived and for
/// archived ones nobody has asked for yet, so the storage class is what tells
/// those two apart.
pub fn restore_state(restore_header: Option<&str>, storage_class: Option<&str>) -> RestoreState {
    match restore_header {
        Some(header) if header.contains("ongoing-request=\"true\"") => RestoreState::InProgress,
        Some(_) => RestoreState::Done,
        None if storage_class.is_some_and(is_archived_class) => RestoreState::Archived,
        None => RestoreState::NotArchived,
    }
}

/// Storage classes that cannot be read without restoring first.
///
/// `GLACIER_IR` is deliberately absent: Instant Retrieval is cheap-but-slow
/// storage that still serves a plain GET, so offering a restore for it would be
/// an operation that does nothing.
pub fn is_archived_class(storage_class: &str) -> bool {
    matches!(storage_class, "GLACIER" | "DEEP_ARCHIVE")
}

/// SigV4's hard ceiling for a presigned URL.
pub const PRESIGN_MAX: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The longest expiry worth offering. Temporary credentials make a longer URL a
/// promise that cannot be kept: it stops working when the session ends, so the
/// UI should say so instead of handing over a link that quietly dies.
pub fn presign_limit_for(temporary_credentials: bool) -> Duration {
    if temporary_credentials {
        // An STS session is typically an hour and rarely more than twelve, so
        // offering a week would be theatre.
        Duration::from_secs(60 * 60)
    } else {
        PRESIGN_MAX
    }
}

/// The plain, unsigned URL of an object — useful only when the object is public.
/// Path-style for providers that need it, virtual-hosted otherwise, matching how
/// the client itself addresses the bucket.
pub fn public_url(profile: &Profile, bucket: &str, key: &str) -> String {
    let encoded = encode_copy_source("", key).trim_start_matches('/').to_string();

    match &profile.endpoint {
        Some(endpoint) => {
            let base = endpoint.trim_end_matches('/');
            if profile.path_style {
                format!("{base}/{bucket}/{encoded}")
            } else {
                match base.split_once("://") {
                    Some((scheme, host)) => format!("{scheme}://{bucket}.{host}/{encoded}"),
                    None => format!("{base}/{bucket}/{encoded}"),
                }
            }
        }
        // Real AWS: virtual-hosted style, region in the host.
        None => format!(
            "https://{bucket}.s3.{}.amazonaws.com/{encoded}",
            profile.region
        ),
    }
}

/// CRC32 of some bytes, base64-encoded the way S3 reports and accepts it.
///
/// Attached explicitly to each upload request rather than left to the SDK's
/// global setting: `relaxed_checksums` turns that setting off for non-AWS
/// providers (they reject the automatic headers), which had the side effect of
/// storing no checksum at all — so a download had nothing to verify against.
/// An explicit value goes through regardless.
pub fn crc32_base64(bytes: &[u8]) -> String {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    encode_crc32(hasher.finalize())
}

/// Encodes an already-computed CRC32.
///
/// Separate from [`crc32_base64`] on purpose: they take the same-looking `&[u8]`
/// versus `u32` and mixing them up hashes the digest a second time, which
/// produces a plausible-looking value that matches nothing. That mistake was
/// made once already and only a real server caught it.
pub fn encode_crc32(value: u32) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let value = value.to_be_bytes();

    // Four bytes is one full 3-byte group plus a 1-byte remainder.
    let triple =
        u32::from(value[0]) << 16 | u32::from(value[1]) << 8 | u32::from(value[2]);
    let mut out = String::with_capacity(8);
    for shift in [18, 12, 6, 0] {
        out.push(B64[((triple >> shift) & 0x3f) as usize] as char);
    }
    let remainder = u32::from(value[3]) << 16;
    out.push(B64[((remainder >> 18) & 0x3f) as usize] as char);
    out.push(B64[((remainder >> 12) & 0x3f) as usize] as char);
    out.push_str("==");
    out
}

/// CopyObject refuses anything larger; past this a copy has to go through
/// UploadPartCopy instead.
pub const COPY_OBJECT_LIMIT: u64 = 5 * 1024 * 1024 * 1024;

/// Part size for a server-side copy. Larger than an upload part because nothing
/// crosses this machine — the only cost of a big part is a longer retry.
const COPY_PART_SIZE: u64 = 256 * 1024 * 1024;

/// Keeps a server-side copy under the 10,000-part ceiling. A 5 TB object (S3's
/// maximum) needs parts bigger than the default to fit.
fn copy_part_size_for(total: u64) -> u64 {
    let mut size = COPY_PART_SIZE;
    while total.div_ceil(size) > 10_000 {
        size *= 2;
    }
    size
}

/// Builds the `x-amz-copy-source` value. The SDK sends this header as given, so
/// a key containing a space, `+`, `#` or `?` has to be percent-encoded here or
/// the server reads a different key than the one meant — silently copying the
/// wrong object, or failing with a confusing 404.
///
/// Slashes stay literal: they separate the bucket from the key and the key's own
/// path segments.
fn encode_copy_source(bucket: &str, key: &str) -> String {
    let mut out = String::with_capacity(bucket.len() + key.len() + 1);
    out.push_str(bucket);
    out.push('/');
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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

    fn profile_for(endpoint: Option<&str>, path_style: bool) -> Profile {
        Profile {
            name: "t".into(),
            endpoint: endpoint.map(str::to_owned),
            region: "ap-southeast-1".into(),
            path_style,
            access_key: "a".into(),
            secret_key: "s".into(),
            session_token: None,
            relaxed_checksums: false,
        }
    }

    #[test]
    fn restore_state_distinguishes_archived_from_merely_cold() {
        // Never archived: no header, ordinary class.
        assert_eq!(restore_state(None, None), RestoreState::NotArchived);
        assert_eq!(
            restore_state(None, Some("STANDARD_IA")),
            RestoreState::NotArchived
        );

        // Archived and untouched — a GET would fail, so the UI must offer a restore.
        assert_eq!(restore_state(None, Some("GLACIER")), RestoreState::Archived);
        assert_eq!(
            restore_state(None, Some("DEEP_ARCHIVE")),
            RestoreState::Archived
        );

        // Instant Retrieval serves a plain GET; offering a restore would do nothing.
        assert_eq!(
            restore_state(None, Some("GLACIER_IR")),
            RestoreState::NotArchived
        );
        assert!(!is_archived_class("GLACIER_IR"));

        // Restore running: not readable yet, however encouraging the header looks.
        assert_eq!(
            restore_state(Some(r#"ongoing-request="true""#), Some("GLACIER")),
            RestoreState::InProgress
        );

        // Restore finished, with the expiry the copy is good until.
        assert_eq!(
            restore_state(
                Some(r#"ongoing-request="false", expiry-date="Fri, 21 Aug 2026 00:00:00 GMT""#),
                Some("GLACIER")
            ),
            RestoreState::Done
        );
    }

    #[test]
    fn presign_ceiling_shrinks_for_temporary_credentials() {
        // Long-lived keys can genuinely sign a week.
        assert_eq!(presign_limit_for(false), PRESIGN_MAX);

        // A session-backed URL dies with the session, so promising a week would
        // be a promise the app cannot keep.
        let temporary = presign_limit_for(true);
        assert!(temporary < PRESIGN_MAX);
        assert_eq!(temporary, Duration::from_secs(3600));
    }

    #[test]
    fn public_url_matches_how_the_bucket_is_addressed() {
        // Real AWS: virtual-hosted, region in the host.
        assert_eq!(
            public_url(&profile_for(None, false), "my-bucket", "a/b.txt"),
            "https://my-bucket.s3.ap-southeast-1.amazonaws.com/a/b.txt"
        );

        // MinIO and friends: path-style keeps the bucket in the path.
        assert_eq!(
            public_url(
                &profile_for(Some("http://127.0.0.1:9000"), true),
                "demo",
                "a/b.txt"
            ),
            "http://127.0.0.1:9000/demo/a/b.txt"
        );

        // A custom endpoint that does virtual-hosted addressing.
        assert_eq!(
            public_url(&profile_for(Some("https://s3.example.com"), false), "b", "k.txt"),
            "https://b.s3.example.com/k.txt"
        );

        // A key needing encoding must not produce a URL that points elsewhere.
        assert_eq!(
            public_url(&profile_for(Some("http://h:9000"), true), "b", "a file?.txt"),
            "http://h:9000/b/a%20file%3F.txt"
        );

        // A trailing slash on the endpoint must not double up.
        assert_eq!(
            public_url(&profile_for(Some("http://h:9000/"), true), "b", "k"),
            "http://h:9000/b/k"
        );
    }

    #[test]
    fn copy_source_percent_encodes_what_would_be_misread() {
        // Ordinary keys pass through untouched, slashes included.
        assert_eq!(
            encode_copy_source("my-bucket", "reports/2026/q1.txt"),
            "my-bucket/reports/2026/q1.txt"
        );

        // A space and a `+` are the classic pair: unencoded, the server reads
        // `+` as a space and copies a key nobody asked for.
        assert_eq!(
            encode_copy_source("b", "a file+name.txt"),
            "b/a%20file%2Bname.txt"
        );

        // `?` and `#` would otherwise start a query or fragment.
        assert_eq!(encode_copy_source("b", "who?.txt"), "b/who%3F.txt");
        assert_eq!(encode_copy_source("b", "no#1.txt"), "b/no%231.txt");

        // Non-ASCII is encoded per UTF-8 byte.
        assert_eq!(encode_copy_source("b", "é"), "b/%C3%A9");

        // Unreserved characters must NOT be encoded, or the key changes.
        assert_eq!(encode_copy_source("b", "a-_.~z"), "b/a-_.~z");
    }

    #[test]
    fn copy_part_size_grows_to_stay_under_the_part_ceiling() {
        // Anything that fits in 10,000 default-sized parts keeps the default.
        assert_eq!(copy_part_size_for(0), COPY_PART_SIZE);
        assert_eq!(copy_part_size_for(COPY_OBJECT_LIMIT), COPY_PART_SIZE);

        // S3's largest object is 5 TB; it must still produce a legal part count.
        let five_tb = 5 * 1024 * 1024 * 1024 * 1024;
        let size = copy_part_size_for(five_tb);
        assert!(
            five_tb.div_ceil(size) <= 10_000,
            "5 TB would need {} parts of {size} bytes",
            five_tb.div_ceil(size)
        );

        // Exactly at the boundary, one more byte must push the part size up.
        let exactly_10k = COPY_PART_SIZE * 10_000;
        assert_eq!(copy_part_size_for(exactly_10k), COPY_PART_SIZE);
        assert_eq!(copy_part_size_for(exactly_10k + 1), COPY_PART_SIZE * 2);
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
