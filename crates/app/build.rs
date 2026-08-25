//! Embeds `resources/windows/s3browser.ico` as icon resource 1 in the .exe.
//!
//! gpui's own `load_icon()` reads exactly that resource ID from the running
//! module (vendor/gpui/src/platform/windows/platform.rs); gpui's .rc embeds
//! only its manifest, so without this the window, taskbar entry and Explorer
//! all fall back to a blank icon on Windows. See
//! `resources/windows/s3browser.rc` for the full story.

fn main() {
    #[cfg(target_os = "windows")]
    embed_icon();
}

#[cfg(target_os = "windows")]
fn embed_icon() {
    let rc_file = std::path::Path::new("resources/windows/s3browser.rc");
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed=resources/windows/s3browser.ico");

    embed_resource::compile(rc_file, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
