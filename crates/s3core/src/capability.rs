//! What a provider can actually do.
//!
//! "S3-compatible" is a spectrum. R2 refuses `ListBuckets` for a scoped token,
//! MinIO answers `NotImplemented` for server-side encryption without a KMS
//! backend, and several providers have no object tagging at all. Guessing from
//! the endpoint hostname gets the common cases right and the interesting ones
//! wrong — the point of this module is to ask instead.
//!
//! Detection is per bucket, not per provider: versioning and object lock are
//! bucket settings, and a token may reach one bucket and not another.
//!
//! Every probe is a cheap read that either answers or refuses, and a refusal is
//! the answer. The one thing a probe must never do is write.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::S3Client;

/// One capability, and why it might be missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Yes,
    /// The provider does not implement it.
    No,
    /// It may exist, but these credentials cannot see it. Different from `No`:
    /// the fix is a wider token, not a different provider.
    Forbidden,
}

impl Support {
    /// Whether the UI should offer the feature at all.
    pub fn is_usable(self) -> bool {
        self == Support::Yes
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub versioning: Support,
    pub tagging: Support,
    pub lifecycle: Support,
    pub object_lock: Support,
    /// Whether ACLs can be *read*. Writing them is a separate permission that
    /// no read-only probe can establish — MinIO answers `GetBucketAcl` and then
    /// refuses `PutObjectAcl`. So this gates showing the panel, and a failed
    /// write still has to be reported rather than assumed impossible.
    ///
    /// ACLs are off by default on buckets created since 2023 (Object Ownership
    /// set to BucketOwnerEnforced), and some providers never had them. Both
    /// answer `AccessControlListNotSupported`.
    pub acl: Support,
}

/// Classifies the error from a probe.
///
/// The distinction that matters: a provider saying "I do not implement this"
/// versus "you may not ask". Only the first should grey a feature out for
/// everyone; the second is about the token in use.
fn classify(error: &str) -> Support {
    // `NoSuchLifecycleConfiguration` and friends mean the feature exists and is
    // simply unset — that is a yes, not a no.
    if error.contains("NoSuch") || error.contains("NotFound") {
        return Support::Yes;
    }
    if error.contains("NotImplemented")
        || error.contains("MethodNotAllowed")
        // What a bucket with Object Ownership set to BucketOwnerEnforced says,
        // and what providers without ACLs say. Both mean the same to the UI.
        || error.contains("AccessControlListNotSupported")
    {
        return Support::No;
    }
    if error.contains("AccessDenied") || error.contains("Forbidden") {
        return Support::Forbidden;
    }
    // An unrecognised failure is not evidence the feature is missing; assuming
    // it is would hide a working feature behind a transient network error.
    Support::Yes
}

/// Caches what has been detected, so opening a bucket twice costs one round of
/// probes rather than two.
#[derive(Clone, Default)]
pub struct CapabilityCache {
    entries: Arc<Mutex<HashMap<String, Capabilities>>>,
}

impl CapabilityCache {
    pub fn get(&self, bucket: &str) -> Option<Capabilities> {
        self.entries.lock().unwrap().get(bucket).copied()
    }

    pub fn insert(&self, bucket: &str, capabilities: Capabilities) {
        self.entries
            .lock()
            .unwrap()
            .insert(bucket.to_string(), capabilities);
    }

    /// Forgets everything. Called when the credentials change, because
    /// `Forbidden` is a fact about a token and not about the bucket.
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

impl S3Client {
    /// Probes a bucket, four cheap reads. Cached by the caller.
    pub async fn detect_capabilities(&self, bucket: &str) -> Capabilities {
        Capabilities {
            versioning: self.probe_versioning(bucket).await,
            tagging: self.probe_tagging(bucket).await,
            lifecycle: self.probe_lifecycle(bucket).await,
            object_lock: self.probe_object_lock(bucket).await,
            acl: self.probe_acl(bucket).await,
        }
    }

    async fn probe_versioning(&self, bucket: &str) -> Support {
        match self
            .inner()
            .get_bucket_versioning()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(_) => Support::Yes,
            Err(error) => classify(&format!("{error:?}")),
        }
    }

    async fn probe_tagging(&self, bucket: &str) -> Support {
        match self
            .inner()
            .get_bucket_tagging()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(_) => Support::Yes,
            Err(error) => classify(&format!("{error:?}")),
        }
    }

    async fn probe_lifecycle(&self, bucket: &str) -> Support {
        match self
            .inner()
            .get_bucket_lifecycle_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(_) => Support::Yes,
            Err(error) => classify(&format!("{error:?}")),
        }
    }

    async fn probe_acl(&self, bucket: &str) -> Support {
        match self.inner().get_bucket_acl().bucket(bucket).send().await {
            Ok(_) => Support::Yes,
            Err(error) => classify(&format!("{error:?}")),
        }
    }

    async fn probe_object_lock(&self, bucket: &str) -> Support {
        match self
            .inner()
            .get_object_lock_configuration()
            .bucket(bucket)
            .send()
            .await
        {
            Ok(_) => Support::Yes,
            Err(error) => classify(&format!("{error:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_not_unsupported() {
        // The trap this classifier exists for: an empty lifecycle config comes
        // back as an error, and reading that as "no lifecycle support" would
        // grey out a feature that works perfectly.
        assert_eq!(classify("NoSuchLifecycleConfiguration"), Support::Yes);
        assert_eq!(
            classify("ObjectLockConfigurationNotFoundError"),
            Support::Yes
        );
    }

    #[test]
    fn not_implemented_and_denied_are_different_answers() {
        // The provider does not have it: nobody can use it, grey it out.
        assert_eq!(classify("NotImplemented"), Support::No);
        assert_eq!(classify("MethodNotAllowed"), Support::No);
        // A bucket with ACLs switched off answers this, and so does a provider
        // that never had them; the control disappears either way.
        assert_eq!(classify("AccessControlListNotSupported"), Support::No);
        assert!(!Support::No.is_usable());

        // The token may not ask: the feature may be fine, the fix is a wider
        // token. Telling the user "your provider does not support this" here
        // would send them looking in the wrong place entirely.
        assert_eq!(classify("AccessDenied"), Support::Forbidden);
        assert!(!Support::Forbidden.is_usable());
    }

    #[test]
    fn an_unknown_failure_does_not_disable_a_feature() {
        // A timeout or a DNS blip is not evidence about capabilities. Assuming
        // the worst would hide working features behind a transient error.
        assert_eq!(classify("connection reset by peer"), Support::Yes);
        assert_eq!(classify(""), Support::Yes);
    }

    #[test]
    fn cache_is_per_bucket_and_clearable() {
        let cache = CapabilityCache::default();
        assert!(cache.get("a").is_none());

        cache.insert(
            "a",
            Capabilities {
                versioning: Support::Yes,
                tagging: Support::No,
                lifecycle: Support::Yes,
                object_lock: Support::No,
                acl: Support::No,
            },
        );
        assert_eq!(cache.get("a").unwrap().tagging, Support::No);
        // A different bucket is a different answer, not a cache hit.
        assert!(cache.get("b").is_none());

        // Credentials changing invalidates everything, because `Forbidden` is a
        // fact about the token rather than about the bucket.
        cache.clear();
        assert!(cache.get("a").is_none());
    }
}
