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
pub use aws_config_file::{
    import_aws_profiles, parse_aws_files, system_default_region, ImportedProfile,
};

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

/// A provider preset: the endpoint shape and region a service expects.
///
/// Only a starting point for the two fields nobody can be expected to remember.
/// The quirks — path-style addressing, checksum mode — are still derived from
/// the finished endpoint by [`StoredProfile::with_provider_defaults`], because a
/// second place that knows which provider needs what would drift from the first
/// and the drift would look like a credentials problem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Aws,
    R2,
    B2,
    Wasabi,
    Spaces,
    MinIO,
    /// Anything else that speaks S3. Defaults to what self-hosted stores
    /// overwhelmingly need: path-style addressing and relaxed checksums.
    Compatible,
}

impl Provider {
    pub const ALL: [Provider; 7] = [
        Provider::Aws,
        Provider::R2,
        Provider::B2,
        Provider::Wasabi,
        Provider::Spaces,
        Provider::MinIO,
        Provider::Compatible,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Aws => "AWS",
            Provider::R2 => "Cloudflare R2",
            Provider::B2 => "Backblaze B2",
            Provider::Wasabi => "Wasabi",
            Provider::Spaces => "DO Spaces",
            Provider::MinIO => "MinIO",
            Provider::Compatible => "S3 Compatible",
        }
    }

    /// The provider a label names. The dropdown hands back the label it showed,
    /// and this is the only place that mapping lives.
    pub fn from_label(label: &str) -> Option<Provider> {
        Provider::ALL
            .into_iter()
            .find(|provider| provider.label() == label)
    }

    /// What to put in the endpoint field. Empty for AWS, which addresses its
    /// regions by name and needs no endpoint at all.
    ///
    /// The templates carry a visibly fake segment where the account or region
    /// goes, so a half-filled endpoint fails loudly at connect time instead of
    /// quietly pointing somewhere real.
    pub fn endpoint_template(self) -> &'static str {
        match self {
            Provider::Aws => "",
            Provider::R2 => "https://ACCOUNT_ID.r2.cloudflarestorage.com",
            Provider::B2 => "https://s3.us-west-004.backblazeb2.com",
            Provider::Wasabi => "https://s3.wasabisys.com",
            Provider::Spaces => "https://sgp1.digitaloceanspaces.com",
            Provider::MinIO => "http://127.0.0.1:9000",
            // Deliberately not a real host, for the same reason as R2's.
            Provider::Compatible => "https://s3.example.com",
        }
    }

    pub fn region_template(self) -> &'static str {
        match self {
            // R2 has one namespace and rejects a real region name.
            Provider::R2 => "auto",
            Provider::B2 => "us-west-004",
            Provider::Spaces => "sgp1",
            _ => "us-east-1",
        }
    }
}

/// A location worth getting back to.
///
/// Carries the profile it belongs to: the same `bucket/prefix` under two
/// profiles is two different places, and offering one from the wrong profile
/// would open a bucket the credentials cannot see.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Place {
    pub profile: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    /// When it was last visited, in epoch seconds.
    ///
    /// Defaulted rather than required: a `places.json` written before this
    /// field existed still loads, and its entries come back as zero — which the
    /// UI shows as no time at all rather than as 1970.
    #[serde(default)]
    pub at: i64,
}

/// Equality is *where*, not *when*.
///
/// Written out rather than derived because `at` must stay out of it. Two visits
/// to the same prefix are the same place, and the list's whole job depends on
/// that: `visit` drops the old entry before inserting the new one, and pinning
/// looks a place up by identity. Derive `PartialEq` over `at` and every revisit
/// becomes a second row, until the list holds one prefix twelve times.
impl PartialEq for Place {
    fn eq(&self, other: &Self) -> bool {
        self.profile == other.profile && self.bucket == other.bucket && self.prefix == other.prefix
    }
}

impl Eq for Place {}

impl Place {
    /// `bucket/prefix`, for showing in a list one line high.
    pub fn label(&self) -> String {
        if self.prefix.is_empty() {
            self.bucket.clone()
        } else {
            format!("{}/{}", self.bucket, self.prefix.trim_end_matches('/'))
        }
    }
}

/// Pinned and recently visited locations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Places {
    #[serde(default)]
    pub favorites: Vec<Place>,
    /// Newest first.
    #[serde(default)]
    pub recent: Vec<Place>,
}

/// How many places back the list remembers.
///
/// Was twelve while this lived in the sidebar, where the limit was really about
/// how tall a sidebar may get. It has a page of its own now, so the number can
/// be about memory instead: enough to cover more than one sitting.
pub const RECENT_LIMIT: usize = 50;

impl Places {
    /// Records a visit, newest first, without repeating one already there.
    ///
    /// Moving a repeat to the front rather than adding a second copy: a list
    /// that fills up with the same prefix has nothing older left in it, which
    /// is the one thing it is for.
    pub fn visit(&mut self, place: Place) {
        self.recent.retain(|existing| existing != &place);
        self.recent.insert(0, place);
        self.recent.truncate(RECENT_LIMIT);
    }

    pub fn is_favorite(&self, place: &Place) -> bool {
        self.favorites.contains(place)
    }

    /// Pins or unpins. Returns whether it is pinned afterwards.
    pub fn toggle_favorite(&mut self, place: Place) -> bool {
        if let Some(index) = self
            .favorites
            .iter()
            .position(|existing| existing == &place)
        {
            self.favorites.remove(index);
            false
        } else {
            self.favorites.push(place);
            true
        }
    }

    /// Only what the given profile can actually open.
    pub fn for_profile<'a>(&'a self, profile: &'a str) -> impl Iterator<Item = &'a Place> + 'a {
        self.favorites
            .iter()
            .filter(move |place| place.profile == profile)
    }

    pub fn recent_for_profile<'a>(
        &'a self,
        profile: &'a str,
    ) -> impl Iterator<Item = &'a Place> + 'a {
        self.recent
            .iter()
            .filter(move |place| place.profile == profile)
    }
}

/// Reads and writes `places.json`, beside the profiles.
pub struct PlaceStore {
    path: PathBuf,
}

impl PlaceStore {
    pub fn beside(profiles: &Path) -> Self {
        Self {
            path: profiles.with_file_name("places.json"),
        }
    }

    /// A missing or unreadable file is an empty list, not an error: favourites
    /// are a convenience, and refusing to start the app over one is not a trade
    /// anybody wants.
    pub fn load(&self) -> Places {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, places: &Places) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(places)?;
        fs::write(&self.path, text).with_context(|| format!("writing {}", self.path.display()))
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
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
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
        assert_eq!(
            resolve_dev_secret(secret(), true).as_deref(),
            Some("minioadmin")
        );

        // In a shipped binary it must not exist at all. A credential taken from
        // the environment defeats the credential store: whoever can set a
        // variable on the process picks the key it signs with.
        assert_eq!(resolve_dev_secret(secret(), false), None);

        // Unset and empty both mean "use the store", not "sign with nothing".
        assert_eq!(resolve_dev_secret(None, true), None);
        assert_eq!(resolve_dev_secret(Some(String::new()), true), None);
    }

    fn place(bucket: &str, prefix: &str) -> Place {
        Place {
            profile: "p1".into(),
            bucket: bucket.into(),
            prefix: prefix.into(),
            at: 0,
        }
    }

    #[test]
    fn revisiting_moves_a_place_up_rather_than_adding_it_twice() {
        let mut places = Places::default();
        places.visit(place("a", ""));
        places.visit(place("b", "x/"));
        places.visit(place("a", ""));

        // A list that fills with the same prefix has nothing older left in it,
        // which is the one thing it is for.
        assert_eq!(places.recent, vec![place("a", ""), place("b", "x/")]);

        // And it stops growing.
        for i in 0..RECENT_LIMIT * 2 {
            places.visit(place(&format!("bucket{i}"), ""));
        }
        assert_eq!(places.recent.len(), RECENT_LIMIT);
    }

    #[test]
    fn a_place_belongs_to_the_profile_that_can_open_it() {
        let mut places = Places::default();
        places.favorites.push(place("a", ""));
        places.favorites.push(Place {
            profile: "p2".into(),
            bucket: "b".into(),
            prefix: String::new(),
            at: 0,
        });

        // The same `bucket/prefix` under two profiles is two different places,
        // and offering one from the wrong profile opens a bucket the
        // credentials cannot see.
        let mine: Vec<_> = places.for_profile("p1").collect();
        assert_eq!(mine, vec![&place("a", "")]);
    }

    #[test]
    fn when_is_not_part_of_where() {
        // `at` must stay out of equality. A revisit carries a new timestamp,
        // and if that made it a different place, `visit` would stop replacing
        // the old entry — the list would fill with one prefix over and over
        // until nothing older was left in it, which is the one thing it is for.
        let mut places = Places::default();
        places.visit(Place {
            at: 100,
            ..place("a", "x/")
        });
        places.visit(Place {
            at: 900,
            ..place("a", "x/")
        });
        assert_eq!(places.recent.len(), 1);
        // And the newer visit is the one kept, so the page shows when you were
        // last there rather than when you first were.
        assert_eq!(places.recent[0].at, 900);

        // Pinning looks a place up by identity too, so a pin made at one moment
        // still matches the same place visited at another.
        assert!(places.toggle_favorite(Place {
            at: 100,
            ..place("a", "x/")
        }));
        assert!(places.is_favorite(&Place {
            at: 5_000,
            ..place("a", "x/")
        }));
    }

    #[test]
    fn pinning_is_a_toggle() {
        let mut places = Places::default();
        assert!(places.toggle_favorite(place("a", "x/")));
        assert!(places.is_favorite(&place("a", "x/")));
        assert!(!places.toggle_favorite(place("a", "x/")));
        assert!(places.favorites.is_empty());
    }

    #[test]
    fn a_place_reads_as_a_path() {
        assert_eq!(place("demo", "").label(), "demo");
        assert_eq!(place("demo", "photos/2026/").label(), "demo/photos/2026");
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
    fn every_preset_endpoint_produces_that_providers_quirks() {
        // The presets and `with_provider_defaults` are two halves of the same
        // knowledge, and this is what stops them drifting: a template that no
        // longer matches its own matcher would leave someone with a profile
        // that looks right and cannot connect.
        for provider in Provider::ALL {
            let endpoint = provider.endpoint_template();
            let stored = StoredProfile {
                endpoint: (!endpoint.is_empty()).then(|| endpoint.to_string()),
                region: provider.region_template().into(),
                ..profile("p", None)
            }
            .with_provider_defaults();

            match provider {
                // No endpoint, so virtual-host addressing and real checksums.
                Provider::Aws => {
                    assert!(!stored.path_style);
                    assert!(!stored.relaxed_checksums);
                }
                // R2 answers on a virtual-host style host and insists on `auto`.
                Provider::R2 => {
                    assert!(!stored.path_style, "R2 is not path-style");
                    assert_eq!(stored.region, "auto");
                    assert!(stored.relaxed_checksums);
                }
                Provider::B2 | Provider::Wasabi | Provider::Spaces => {
                    assert!(!stored.path_style, "{provider:?} is not path-style");
                    assert!(stored.relaxed_checksums);
                }
                // Self-hosted and unknown: path-style, because there is no
                // wildcard DNS in front of either.
                Provider::MinIO | Provider::Compatible => {
                    assert!(stored.path_style, "{provider:?} needs path-style");
                    assert!(stored.relaxed_checksums);
                }
            }
        }
    }

    #[test]
    fn a_half_filled_template_is_visibly_unfinished() {
        // The account id has to be replaced by hand. A template that looked
        // like a working hostname would connect somewhere real and fail with a
        // permissions error, which sends people to check the wrong thing.
        assert!(Provider::R2.endpoint_template().contains("ACCOUNT_ID"));
        assert!(Provider::Aws.endpoint_template().is_empty());
        assert!(Provider::Compatible
            .endpoint_template()
            .contains("example.com"));
    }

    #[test]
    fn every_label_maps_back_to_its_provider() {
        // The dropdown returns the string it displayed, so a label that does
        // not round-trip silently picks nothing and leaves the fields as they
        // were — which reads as a dead control rather than as a bug.
        for provider in Provider::ALL {
            assert_eq!(Provider::from_label(provider.label()), Some(provider));
        }
        assert_eq!(Provider::from_label("Không có thật"), None);
    }

    #[test]
    fn provider_defaults_follow_the_endpoint() {
        let minio = profile("m", Some("http://127.0.0.1:9000")).with_provider_defaults();
        assert!(minio.path_style, "self-hosted stores need path-style");
        assert!(minio.relaxed_checksums);

        let r2 =
            profile("r", Some("https://acc.r2.cloudflarestorage.com")).with_provider_defaults();
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
