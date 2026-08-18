//! Seeds the local MinIO profile and verifies the OS credential store works.
//!
//! Useful when bootstrapping a machine (`cargo run -p vault --example dev_profile`)
//! and as a first diagnostic when the app reports that it cannot read a secret:
//! it exercises write, read and delete against Keychain / Credential Manager /
//! Secret Service exactly the way the app does.

use vault::{new_profile_id, ProfileStore, StoredProfile};

const MINIO_SECRET: &str = "minioadmin";

fn main() -> anyhow::Result<()> {
    // Probe first, so a broken credential store fails loudly before we write
    // a profile that could never be connected.
    vault::set_secret_key("dev-profile-probe", "probe")?;
    let read_back = vault::secret_key("dev-profile-probe")?;
    anyhow::ensure!(read_back == "probe", "credential store returned wrong value");
    vault::delete_secret_key("dev-profile-probe")?;
    println!("credential store OK (write, read, delete)");

    let store = ProfileStore::default_location()?;
    let mut profiles = store.load()?;

    if let Some(existing) = profiles.iter().find(|p| p.name == "MinIO local") {
        println!("profile already present: {} ({})", existing.name, existing.id);
        return Ok(());
    }

    let profile = StoredProfile {
        id: new_profile_id("MinIO local", &profiles),
        name: "MinIO local".into(),
        endpoint: Some("http://127.0.0.1:9000".into()),
        region: "us-east-1".into(),
        path_style: true,
        relaxed_checksums: true,
        access_key: "minioadmin".into(),
    };

    vault::set_secret_key(&profile.id, MINIO_SECRET)?;
    println!("stored secret for {}", profile.id);

    profiles.push(profile);
    store.save(&profiles)?;
    println!("wrote {}", store.path().display());

    Ok(())
}
