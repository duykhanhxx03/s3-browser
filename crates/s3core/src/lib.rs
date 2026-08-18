//! Thin, UI-agnostic wrapper over `aws-sdk-s3`.
//!
//! Everything here is plain async Rust with no GPUI types, so it can be unit
//! tested without a window and reused by a future CLI.

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::Client;

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
    pub last_modified: Option<String>,
    pub storage_class: Option<String>,
}

/// One page of a listing. S3 caps a page at 1000 keys, so the UI keeps
/// requesting while `continuation` is `Some`.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub continuation: Option<String>,
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

        let mut builder = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(profile.path_style);

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
                last_modified: None,
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
                last_modified: obj.last_modified().map(|t| t.to_string()),
                storage_class: obj.storage_class().map(|s| s.as_str().to_string()),
            });
        }

        Ok(Page {
            entries,
            continuation: out.next_continuation_token().map(str::to_owned),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
