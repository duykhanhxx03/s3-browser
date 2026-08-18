//! Imports profiles from the shared AWS config files.
//!
//! Users who already work with the AWS CLI expect their profiles to show up
//! without retyping keys, which is table stakes in every commercial client.
//! The two files use slightly different section conventions:
//! `~/.aws/credentials` names sections after the profile, while `~/.aws/config`
//! prefixes everything except `default` with `profile `.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::StoredProfile;

/// A profile found in the AWS files, with its secret still in hand so the caller
/// can decide when to put it in the OS credential store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedProfile {
    pub profile: StoredProfile,
    pub secret_key: String,
}

/// Reads `~/.aws/credentials` and `~/.aws/config`, honouring the standard
/// `AWS_SHARED_CREDENTIALS_FILE` / `AWS_CONFIG_FILE` overrides. Missing files are
/// not an error — most machines have neither, one, or both.
pub fn import_aws_profiles() -> Result<Vec<ImportedProfile>> {
    let home = dirs::home_dir();

    let credentials_path = std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".aws").join("credentials")));
    let config_path = std::env::var_os("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".aws").join("config")));

    let credentials = credentials_path
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let config = config_path
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();

    Ok(parse_aws_files(&credentials, &config))
}

/// The pure part, so the parsing rules are testable without touching a home
/// directory.
pub fn parse_aws_files(credentials: &str, config: &str) -> Vec<ImportedProfile> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    for (name, settings) in parse_ini(credentials) {
        sections.entry(name).or_default().extend(settings);
    }
    // `[profile work]` in config is the same profile as `[work]` in credentials.
    for (name, settings) in parse_ini(config) {
        let name = name
            .strip_prefix("profile ")
            .map(str::trim)
            .unwrap_or(&name)
            .to_string();
        sections.entry(name).or_default().extend(settings);
    }

    let mut imported = Vec::new();
    for (name, settings) in sections {
        // Only profiles with static keys can be used as-is; SSO and assume-role
        // profiles need the flows built in M4, so they are skipped for now.
        let (Some(access_key), Some(secret_key)) = (
            settings.get("aws_access_key_id"),
            settings.get("aws_secret_access_key"),
        ) else {
            continue;
        };

        let profile = StoredProfile {
            id: String::new(), // assigned by the caller against existing profiles
            name: name.clone(),
            endpoint: settings.get("endpoint_url").cloned(),
            region: settings
                .get("region")
                .cloned()
                .unwrap_or_else(|| "us-east-1".into()),
            path_style: false,
            relaxed_checksums: false,
            access_key: access_key.clone(),
        }
        .with_provider_defaults();

        imported.push(ImportedProfile {
            profile,
            secret_key: secret_key.clone(),
        });
    }
    imported
}

/// Minimal INI reader for the AWS file dialect: `#`/`;` comments, `[section]`
/// headers, `key = value` pairs. Nested sub-settings (indented blocks such as
/// `s3 =` followed by indented keys) are ignored rather than misparsed.
fn parse_ini(text: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let mut sections: Vec<(String, BTreeMap<String, String>)> = Vec::new();

    for raw_line in text.lines() {
        let is_nested = raw_line.starts_with(' ') || raw_line.starts_with('\t');
        let line = raw_line.trim();

        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_string(), BTreeMap::new()));
            continue;
        }

        if is_nested {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some((_, settings)) = sections.last_mut() {
            settings.insert(
                key.trim().to_lowercase(),
                value.trim().trim_matches('"').to_string(),
            );
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_credentials_and_config_for_the_same_profile() {
        let credentials = "\
[default]
aws_access_key_id = AKIADEFAULT
aws_secret_access_key = secret-default

[work]
aws_access_key_id = AKIAWORK
aws_secret_access_key = secret-work
";
        let config = "\
[default]
region = ap-southeast-1

[profile work]
region = eu-west-1
";
        let imported = parse_aws_files(credentials, config);
        assert_eq!(imported.len(), 2);

        let default = &imported[0];
        assert_eq!(default.profile.name, "default");
        assert_eq!(default.profile.region, "ap-southeast-1");
        assert_eq!(default.secret_key, "secret-default");

        let work = &imported[1];
        assert_eq!(work.profile.name, "work", "'profile work' maps to 'work'");
        assert_eq!(work.profile.region, "eu-west-1");
        assert_eq!(work.profile.access_key, "AKIAWORK");
    }

    #[test]
    fn skips_profiles_without_static_keys() {
        let config = "\
[profile sso-login]
sso_start_url = https://example.awsapps.com/start
region = us-east-1
";
        assert!(
            parse_aws_files("", config).is_empty(),
            "SSO profiles need the M4 flow, not a static-key import"
        );
    }

    #[test]
    fn reads_custom_endpoints_and_applies_provider_defaults() {
        let credentials = "\
[minio]
aws_access_key_id = minioadmin
aws_secret_access_key = minioadmin
endpoint_url = http://127.0.0.1:9000
";
        let imported = parse_aws_files(credentials, "");
        let profile = &imported[0].profile;
        assert_eq!(profile.endpoint.as_deref(), Some("http://127.0.0.1:9000"));
        assert!(profile.path_style, "MinIO endpoints need path-style");
        assert!(profile.relaxed_checksums);
    }

    #[test]
    fn ignores_comments_and_indented_sub_settings() {
        let config = "\
# a comment
[profile tuned]
region = us-west-2
s3 =
  max_concurrent_requests = 20
  addressing_style = path
aws_access_key_id = AKIA1
aws_secret_access_key = s1
";
        let imported = parse_aws_files("", config);
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].profile.region, "us-west-2");
    }
}
