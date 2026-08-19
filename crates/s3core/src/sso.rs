//! AWS IAM Identity Center (SSO) device authorization flow.
//!
//! **Verification status:** this talks to real AWS endpoints and cannot be
//! exercised against MinIO, so the pure logic below is unit tested but the flow
//! as a whole is unverified. Treat it accordingly until someone runs it against
//! a real Identity Center.
//!
//! The shape of the flow, which is the OAuth 2.0 device grant:
//!
//! 1. Register this app as an OIDC client — anonymous, no pre-registration.
//! 2. Start a device authorization; AWS returns a URL and a user code.
//! 3. The person approves in a browser while we poll for a token.
//! 4. With the token, list the accounts and roles they can use.
//! 5. Pick one and exchange it for temporary S3 credentials.

use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use aws_config::BehaviorVersion;

use crate::sts::TemporaryCredentials;

/// How the person is asked to approve, and what we need to keep polling.
#[derive(Clone, Debug)]
pub struct DeviceAuthorization {
    /// Open this; it already carries the code.
    pub verification_uri: String,
    /// Show this too — the URL may be copied by hand, and some browsers strip
    /// the query string.
    pub user_code: String,
    pub device_code: String,
    pub client_id: String,
    pub client_secret: String,
    /// AWS's requested gap between polls. Polling faster earns a slow-down
    /// error, so this is a floor rather than a suggestion.
    pub interval: Duration,
    pub expires_at: SystemTime,
}

/// One role in one account that the signed-in person may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsoRole {
    pub account_id: String,
    pub account_name: String,
    pub role_name: String,
}

impl SsoRole {
    /// What to show in a picker. The account name alone is ambiguous when the
    /// same person can reach several roles in it.
    pub fn label(&self) -> String {
        format!("{} / {} ({})", self.account_name, self.role_name, self.account_id)
    }
}

fn oidc_client(region: &str) -> aws_sdk_ssooidc::Client {
    let config = aws_sdk_ssooidc::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()))
        .build();
    aws_sdk_ssooidc::Client::from_conf(config)
}

fn sso_client(region: &str) -> aws_sdk_sso::Client {
    let config = aws_sdk_sso::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()))
        .build();
    aws_sdk_sso::Client::from_conf(config)
}

/// Registers a client and starts a device authorization.
///
/// `start_url` is the organisation's portal URL and `region` is where Identity
/// Center lives, which is not necessarily where the buckets are.
pub async fn begin(start_url: &str, region: &str) -> Result<DeviceAuthorization> {
    let client = oidc_client(region);

    let registration = client
        .register_client()
        .client_name("s3browser")
        .client_type("public")
        .send()
        .await
        .context("RegisterClient thất bại")?;

    let (Some(client_id), Some(client_secret)) = (
        registration.client_id().map(str::to_owned),
        registration.client_secret().map(str::to_owned),
    ) else {
        bail!("RegisterClient không trả về client id/secret");
    };

    let authorization = client
        .start_device_authorization()
        .client_id(&client_id)
        .client_secret(&client_secret)
        .start_url(start_url)
        .send()
        .await
        .context("StartDeviceAuthorization thất bại")?;

    let device_code = authorization
        .device_code()
        .context("StartDeviceAuthorization không trả về device code")?
        .to_string();
    let verification_uri = authorization
        .verification_uri_complete()
        .or_else(|| authorization.verification_uri())
        .context("StartDeviceAuthorization không trả về URL xác thực")?
        .to_string();

    Ok(DeviceAuthorization {
        verification_uri,
        user_code: authorization.user_code().unwrap_or_default().to_string(),
        device_code,
        client_id,
        client_secret,
        interval: poll_interval(authorization.interval()),
        expires_at: SystemTime::now() + Duration::from_secs(authorization.expires_in().max(0) as u64),
    })
}

/// Polls once for the token. `Ok(None)` means the person has not approved yet
/// and the caller should wait and try again — that is the normal case, not an
/// error, and treating it as one would abort the flow on the first poll.
pub async fn poll_once(auth: &DeviceAuthorization, region: &str) -> Result<Option<String>> {
    let client = oidc_client(region);

    match client
        .create_token()
        .client_id(&auth.client_id)
        .client_secret(&auth.client_secret)
        .grant_type("urn:ietf:params:oauth:grant-type:device_code")
        .device_code(&auth.device_code)
        .send()
        .await
    {
        Ok(out) => out
            .access_token()
            .map(str::to_owned)
            .map(Some)
            .context("CreateToken không trả về access token"),
        Err(error) => {
            let message = format!("{error:?}");
            // Still waiting, or told to back off: both mean keep going.
            if message.contains("AuthorizationPendingException")
                || message.contains("SlowDownException")
            {
                Ok(None)
            } else {
                Err(error).context("CreateToken thất bại")
            }
        }
    }
}

/// Polls until the person approves, the code expires, or something goes wrong.
///
/// The waiting belongs here rather than in the caller: the cadence is AWS's
/// (polling faster than the interval earns a slow-down error), and the deadline
/// is part of the authorization, not of whatever UI happens to be showing it.
pub async fn wait_for_token(auth: &DeviceAuthorization, region: &str) -> Result<String> {
    loop {
        if has_expired(auth, SystemTime::now()) {
            bail!("Mã đăng nhập đã hết hạn, thử lại từ đầu");
        }
        if let Some(token) = poll_once(auth, region).await? {
            return Ok(token);
        }
        tokio::time::sleep(auth.interval).await;
    }
}

/// Every account and role the token can reach, flattened into one list because
/// that is what a picker needs.
pub async fn list_roles(access_token: &str, region: &str) -> Result<Vec<SsoRole>> {
    let client = sso_client(region);
    let mut roles = Vec::new();

    let mut accounts = client
        .list_accounts()
        .access_token(access_token)
        .into_paginator()
        .send();

    let mut listed = Vec::new();
    while let Some(page) = accounts.next().await {
        let page = page.context("ListAccounts thất bại")?;
        for account in page.account_list() {
            if let Some(id) = account.account_id() {
                listed.push((
                    id.to_string(),
                    account.account_name().unwrap_or(id).to_string(),
                ));
            }
        }
    }

    for (account_id, account_name) in listed {
        let mut pages = client
            .list_account_roles()
            .access_token(access_token)
            .account_id(&account_id)
            .into_paginator()
            .send();

        while let Some(page) = pages.next().await {
            let page = page.context("ListAccountRoles thất bại")?;
            for role in page.role_list() {
                if let Some(role_name) = role.role_name() {
                    roles.push(SsoRole {
                        account_id: account_id.clone(),
                        account_name: account_name.clone(),
                        role_name: role_name.to_string(),
                    });
                }
            }
        }
    }
    Ok(roles)
}

/// Exchanges a chosen role for credentials that can talk to S3.
pub async fn credentials_for(
    access_token: &str,
    role: &SsoRole,
    region: &str,
) -> Result<TemporaryCredentials> {
    let out = sso_client(region)
        .get_role_credentials()
        .access_token(access_token)
        .account_id(&role.account_id)
        .role_name(&role.role_name)
        .send()
        .await
        .context("GetRoleCredentials thất bại")?;

    let credentials = out
        .role_credentials()
        .context("GetRoleCredentials không trả về credentials")?;

    Ok(TemporaryCredentials {
        access_key: credentials.access_key_id().unwrap_or_default().to_string(),
        secret_key: credentials
            .secret_access_key()
            .unwrap_or_default()
            .to_string(),
        session_token: credentials.session_token().unwrap_or_default().to_string(),
        // Milliseconds here, unlike every other expiry in the SDK.
        expires_at: Some(
            SystemTime::UNIX_EPOCH + Duration::from_millis(credentials.expiration().max(0) as u64),
        ),
    })
}

/// AWS's polling interval, with a floor. A missing or zero interval must not
/// become a busy loop against an AWS endpoint, which would earn rate limiting
/// and make the flow look broken.
fn poll_interval(interval: i32) -> Duration {
    Duration::from_secs(interval.clamp(1, 60) as u64)
}

/// Whether the device authorization has run out. Polling past this only ever
/// returns an error, so the caller should stop and say the code expired rather
/// than keep going.
pub fn has_expired(auth: &DeviceAuthorization, now: SystemTime) -> bool {
    now >= auth.expires_at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_never_becomes_a_busy_loop() {
        // AWS's usual answer.
        assert_eq!(poll_interval(5), Duration::from_secs(5));

        // Zero or missing must not mean "poll as fast as you can" — that earns
        // rate limiting and makes the flow look broken.
        assert_eq!(poll_interval(0), Duration::from_secs(1));
        assert_eq!(poll_interval(-1), Duration::from_secs(1));

        // Absurdly long intervals are capped so the flow stays responsive.
        assert_eq!(poll_interval(9999), Duration::from_secs(60));
    }

    fn authorization(expires_at: SystemTime) -> DeviceAuthorization {
        DeviceAuthorization {
            verification_uri: "https://device.sso/".into(),
            user_code: "ABCD-EFGH".into(),
            device_code: "device".into(),
            client_id: "id".into(),
            client_secret: "secret".into(),
            interval: Duration::from_secs(5),
            expires_at,
        }
    }

    #[test]
    fn expiry_stops_the_poll_loop() {
        let now = SystemTime::now();
        assert!(!has_expired(
            &authorization(now + Duration::from_secs(60)),
            now
        ));
        assert!(has_expired(
            &authorization(now - Duration::from_secs(1)),
            now
        ));
        // Exactly at the deadline counts as expired: one more poll can only
        // return an error.
        let at = now + Duration::from_secs(30);
        assert!(has_expired(&authorization(at), at));
    }

    #[test]
    fn role_labels_disambiguate_accounts_with_several_roles() {
        let role = SsoRole {
            account_id: "123456789012".into(),
            account_name: "Production".into(),
            role_name: "ReadOnly".into(),
        };
        // Account name alone is ambiguous when one account offers several roles,
        // and the id is what tells two identically named accounts apart.
        assert_eq!(role.label(), "Production / ReadOnly (123456789012)");
    }
}
