//! Connection profiles and their secrets.
//!
//! Non-secret settings live in a JSON file under the platform config directory;
//! the secret access key goes to the OS credential store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux). Nothing here knows
//! about GPUI or the AWS SDK, so it tests without a window or a network.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

mod aws_config_file;
pub use aws_config_file::{import_aws_profiles, parse_aws_files, ImportedProfile};

const KEYRING_SERVICE: &str = "dev.s3browser.credentials";

/// Everything about a connection except the secret key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredProfile {
    /// Stable identifier, also the credential-store key. Never re-used.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub region: String,
    #[serde(default)]
    pub path_style: bool,
    #[serde(default)]
    pub relaxed_checksums: bool,
    pub access_key: String,
}

impl StoredProfile {
    /// Applies the quirk defaults a provider needs, based on its endpoint.
    /// Mirrors what Cyberduck ships as downloadable connection profiles.
    pub fn with_provider_defaults(mut self) -> Self {
        let Some(endpoint) = self.endpoint.as_deref() else {
            // Real AWS: virtual-host addressing, standard checksums.
            self.path_style = false;
            self.relaxed_checksums = false;
            return self;
        };

        let host = endpoint.to_lowercase();
        // Non-AWS endpoints get relaxed checksums: aws-sdk-s3 >= 1.69 sends CRC32
        // headers by default and several providers reject them.
        self.relaxed_checksums = true;

        if host.contains("r2.cloudflarestorage.com") {
            self.region = "auto".into();
            self.path_style = false;
        } else if host.contains("backblazeb2.com")
            || host.contains("wasabisys.com")
            || host.contains("digitaloceanspaces.com")
        {
            self.path_style = false;
        } else {
            // MinIO and most self-hosted stores need path-style addressing.
            self.path_style = true;
        }
        self
    }
}

/// Reads and writes the profile list plus its secrets.
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    /// Uses the platform config directory:
    /// `~/Library/Application Support/s3browser` (macOS),
    /// `%APPDATA%\s3browser` (Windows), `~/.config/s3browser` (Linux).
    pub fn default_location() -> Result<Self> {
        let dir = dirs::config_dir()
            .context("no config directory for this platform")?
            .join("s3browser");
        Ok(Self::at(dir.join("profiles.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<StoredProfile>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid profile JSON", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    pub fn save(&self, profiles: &[StoredProfile]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(profiles)?;
        fs::write(&self.path, text).with_context(|| format!("writing {}", self.path.display()))
    }
}

/// The environment variable that supplies a secret without touching the
/// credential store. Development only — see [`dev_secret`].
pub const DEV_SECRET_VAR: &str = "S3BROWSER_DEV_SECRET";

/// A secret handed in through the environment instead of read from the
/// credential store.
///
/// **Why this exists.** On macOS the Keychain grants access by code signature,
/// and an unsigned debug build gets a new one from every `cargo build`. So each
/// rebuild asks for the login-keychain password before the app can list
/// anything — which is enough friction that running the app to look at a change
/// stops happening, and changes go out unlooked-at.
///
/// **Debug builds only, deliberately.** A shipped binary must never take a
/// credential from its environment: anything able to set a variable on the
/// process could then point it at a server of its choosing without ever
/// touching the credential store, and the store is the whole reason secrets are
/// not in the config file. `cfg!(debug_assertions)` is what separates the two,
/// so the check cannot be forgotten at release time.
fn dev_secret() -> Option<String> {
    resolve_dev_secret(std::env::var(DEV_SECRET_VAR).ok(), cfg!(debug_assertions))
}

/// The decision, split out so both halves can be tested — a `cfg!` cannot be
/// flipped from within one test run.
fn resolve_dev_secret(value: Option<String>, debug_build: bool) -> Option<String> {
    if !debug_build {
        return None;
    }
    // An empty variable is someone clearing it, not a secret that happens to be
    // the empty string; treating it as one would fail to sign with a confusing
    // error rather than falling back to the store.
    value.filter(|secret| !secret.is_empty())
}

/// Reads the secret key for a profile from the OS credential store, or from
/// [`DEV_SECRET_VAR`] in a debug build.
pub fn secret_key(profile_id: &str) -> Result<String> {
    if let Some(secret) = dev_secret() {
        return Ok(secret);
    }
    entry(profile_id)?
        .get_password()
        .with_context(|| format!("no stored secret for profile {profile_id}"))
}

pub fn set_secret_key(profile_id: &str, secret: &str) -> Result<()> {
    entry(profile_id)?
        .set_password(secret)
        .with_context(|| format!("storing secret for profile {profile_id}"))
}

pub fn delete_secret_key(profile_id: &str) -> Result<()> {
    // A deleted profile must not leave its token behind in the keychain.
    _ = set_session_token(profile_id, None);
    match entry(profile_id)?.delete_credential() {
        Ok(()) => Ok(()),
        // Deleting a profile that never had a secret stored is not an error.
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).with_context(|| format!("deleting secret for {profile_id}")),
    }
}

/// Session tokens live under their own keyring entry rather than being packed
/// in with the secret: existing stored secrets stay readable as-is, and a
/// profile that has no token simply has no entry.
fn token_id(profile_id: &str) -> String {
    format!("{profile_id}#session-token")
}

/// `None` when the profile uses long-lived keys, which is the common case.
pub fn session_token(profile_id: &str) -> Option<String> {
    entry(&token_id(profile_id)).ok()?.get_password().ok()
}

pub fn set_session_token(profile_id: &str, token: Option<&str>) -> Result<()> {
    let entry = entry(&token_id(profile_id))?;
    match token {
        Some(token) => entry
            .set_password(token)
            .with_context(|| format!("storing session token for profile {profile_id}")),
        None => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("clearing session token for {profile_id}"))
            }
        },
    }
}

fn entry(profile_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, profile_id)
        .map_err(|error| anyhow!("credential store unavailable: {error}"))
}

/// Generates an id that is unique within `existing`, derived from the name so
/// the credential-store entry is recognizable to a human auditing their keychain.
pub fn new_profile_id(name: &str, existing: &[StoredProfile]) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_matches('-').to_string();
    let base = if base.is_empty() {
        "profile".to_string()
    } else {
        base
    };

    if !existing.iter().any(|p| p.id == base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !existing.iter().any(|p| &p.id == candidate))
        .expect("an unused suffix always exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, endpoint: Option<&str>) -> StoredProfile {
        StoredProfile {
            id: id.into(),
            name: id.into(),
            endpoint: endpoint.map(str::to_owned),
            region: "us-east-1".into(),
            path_style: false,
            relaxed_checksums: false,
            access_key: "AKIA".into(),
        }
    }

    #[test]
    fn the_dev_secret_never_applies_to_a_release_build() {
        let secret = || Some("minioadmin".to_string());

        // In development it is the point of the thing: no Keychain prompt on
        // every rebuild.
        assert_eq!(resolve_dev_secret(secret(), true).as_deref(), Some("minioadmin"));

        // In a shipped binary it must not exist at all. A credential taken from
        // the environment defeats the credential store: whoever can set a
        // variable on the process picks the key it signs with.
        assert_eq!(resolve_dev_secret(secret(), false), None);

        // Unset and empty both mean "use the store", not "sign with nothing".
        assert_eq!(resolve_dev_secret(None, true), None);
        assert_eq!(resolve_dev_secret(Some(String::new()), true), None);
    }

    #[test]
    fn round_trips_profiles_through_disk() {
        let dir = std::env::temp_dir().join(format!("s3browser-test-{}", std::process::id()));
        let store = ProfileStore::at(dir.join("profiles.json"));

        assert!(store.load().unwrap().is_empty(), "missing file reads empty");

        let profiles = vec![profile("minio", Some("http://127.0.0.1:9000"))];
        store.save(&profiles).unwrap();
        assert_eq!(store.load().unwrap(), profiles);

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn provider_defaults_follow_the_endpoint() {
        let minio = profile("m", Some("http://127.0.0.1:9000")).with_provider_defaults();
        assert!(minio.path_style, "self-hosted stores need path-style");
        assert!(minio.relaxed_checksums);

        let r2 = profile("r", Some("https://acc.r2.cloudflarestorage.com")).with_provider_defaults();
        assert_eq!(r2.region, "auto", "R2 uses the 'auto' region");
        assert!(!r2.path_style);
        assert!(r2.relaxed_checksums);

        let aws = profile("a", None).with_provider_defaults();
        assert!(!aws.path_style);
        assert!(
            !aws.relaxed_checksums,
            "real AWS should keep the SDK's integrity checksums on"
        );
    }

    #[test]
    fn ids_stay_unique_and_readable() {
        let existing = vec![profile("my-bucket-host", None)];
        assert_eq!(new_profile_id("Fresh Name", &existing), "fresh-name");
        assert_eq!(
            new_profile_id("My Bucket Host", &existing),
            "my-bucket-host-2",
            "collides with the existing id"
        );
        assert_eq!(new_profile_id("***", &existing), "profile");
    }
}
