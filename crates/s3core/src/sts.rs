//! Temporary credentials via STS AssumeRole.
//!
//! The credentials this hands back expire — an hour by default, and never more
//! than the role's maximum session duration. Everything downstream has to treat
//! them as perishable: a presigned URL cannot outlive them, and a long upload
//! has to be able to re-assume mid-flight.

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;

use crate::Profile;

/// What is needed to assume a role. `external_id` and the MFA pair are optional
/// because most roles need neither, but a role that requires one and does not
/// get it fails with an access-denied error that says nothing about what was
/// missing.
#[derive(Clone, Debug, Default)]
pub struct AssumeRole {
    pub role_arn: String,
    /// Shows up in CloudTrail, so it is worth making it identifiable.
    pub session_name: String,
    /// Required by roles set up for third-party access, to stop the confused
    /// deputy problem.
    pub external_id: Option<String>,
    /// The MFA device's ARN or serial number, paired with a current code.
    pub mfa_serial: Option<String>,
    pub mfa_code: Option<String>,
    /// Requested lifetime. STS clamps this to the role's maximum, so asking for
    /// twelve hours on a one-hour role yields an hour, not an error.
    pub duration: Option<Duration>,
}

/// Credentials with the moment they stop working attached, because the moment
/// is the part callers keep needing and the SDK's own type makes it awkward to
/// reach.
#[derive(Clone, Debug)]
pub struct TemporaryCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: String,
    pub expires_at: Option<SystemTime>,
}

impl TemporaryCredentials {
    /// Whether these are close enough to expiry to be worth replacing now.
    ///
    /// The margin exists because a request signed a second before expiry can
    /// still arrive after it, and because a multipart upload that starts valid
    /// can run long enough to stop being so.
    pub fn expires_within(&self, margin: Duration) -> bool {
        let Some(expires_at) = self.expires_at else {
            // No expiry reported means nothing can be promised about it; treat
            // that as "refresh when asked" rather than "good forever".
            return true;
        };
        match expires_at.duration_since(SystemTime::now()) {
            Ok(remaining) => remaining <= margin,
            // Already past.
            Err(_) => true,
        }
    }
}

/// Assumes a role with the given long-lived credentials.
///
/// `base` supplies the credentials doing the assuming and, for a non-AWS
/// provider, the endpoint — MinIO implements AssumeRole at its own address, so
/// the STS client has to be pointed there rather than at Amazon.
pub async fn assume_role(base: &Profile, request: &AssumeRole) -> Result<TemporaryCredentials> {
    let creds = Credentials::new(
        base.access_key.clone(),
        base.secret_key.clone(),
        base.session_token.clone(),
        None,
        "s3browser-base",
    );

    let sdk_config = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(base.region.clone()))
        .credentials_provider(creds)
        .load()
        .await;

    let mut builder = aws_sdk_sts::config::Builder::from(&sdk_config);
    if let Some(endpoint) = &base.endpoint {
        builder = builder.endpoint_url(endpoint);
    }
    let client = aws_sdk_sts::Client::from_conf(builder.build());

    let mut call = client
        .assume_role()
        .role_arn(&request.role_arn)
        .role_session_name(if request.session_name.is_empty() {
            "s3browser"
        } else {
            &request.session_name
        });

    if let Some(external_id) = &request.external_id {
        call = call.external_id(external_id);
    }
    // Both halves or neither: a serial without a code is rejected by STS with a
    // message that does not mention the code.
    if let (Some(serial), Some(code)) = (&request.mfa_serial, &request.mfa_code) {
        call = call.serial_number(serial).token_code(code);
    }
    if let Some(duration) = request.duration {
        call = call.duration_seconds(duration.as_secs() as i32);
    }

    let out = call
        .send()
        .await
        .with_context(|| format!("AssumeRole failed for {}", request.role_arn))?;

    let credentials = out
        .credentials()
        .context("AssumeRole did not return credentials")?;

    Ok(TemporaryCredentials {
        access_key: credentials.access_key_id().to_string(),
        secret_key: credentials.secret_access_key().to_string(),
        session_token: credentials.session_token().to_string(),
        expires_at: SystemTime::try_from(*credentials.expiration()).ok(),
    })
}

/// The profile to connect with once a role has been assumed: same endpoint and
/// quirks, different credentials.
pub fn profile_with(base: &Profile, credentials: &TemporaryCredentials) -> Profile {
    Profile {
        name: base.name.clone(),
        endpoint: base.endpoint.clone(),
        region: base.region.clone(),
        path_style: base.path_style,
        access_key: credentials.access_key.clone(),
        secret_key: credentials.secret_key.clone(),
        session_token: Some(credentials.session_token.clone()),
        relaxed_checksums: base.relaxed_checksums,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(expires_at: Option<SystemTime>) -> TemporaryCredentials {
        TemporaryCredentials {
            access_key: "a".into(),
            secret_key: "s".into(),
            session_token: "t".into(),
            expires_at,
        }
    }

    #[test]
    fn expiry_margin_treats_the_unknown_as_expiring() {
        let margin = Duration::from_secs(300);

        // Plenty of time left.
        let far = SystemTime::now() + Duration::from_secs(3600);
        assert!(!credentials(Some(far)).expires_within(margin));

        // Inside the margin: still valid this instant, but not for long enough
        // to be worth signing anything new with.
        let soon = SystemTime::now() + Duration::from_secs(60);
        assert!(credentials(Some(soon)).expires_within(margin));

        // Already gone.
        let past = SystemTime::now() - Duration::from_secs(60);
        assert!(credentials(Some(past)).expires_within(margin));

        // No expiry reported: nothing can be promised, so refresh rather than
        // assuming they last forever.
        assert!(credentials(None).expires_within(margin));
    }

    #[test]
    fn assumed_profile_keeps_the_endpoint_and_quirks() {
        let base = Profile {
            name: "work".into(),
            endpoint: Some("http://127.0.0.1:9000".into()),
            region: "ap-southeast-1".into(),
            path_style: true,
            access_key: "old".into(),
            secret_key: "older".into(),
            session_token: None,
            relaxed_checksums: true,
        };
        let assumed = profile_with(&base, &credentials(None));

        // The provider's quirks belong to the endpoint, not to the credentials —
        // losing them here would break MinIO the moment a role is assumed.
        assert_eq!(assumed.endpoint, base.endpoint);
        assert!(assumed.path_style);
        assert!(assumed.relaxed_checksums);
        assert_eq!(assumed.region, base.region);

        // The credentials are the part that changed, token included.
        assert_eq!(assumed.access_key, "a");
        assert_eq!(assumed.session_token.as_deref(), Some("t"));
    }
}
