//! Self-check for the M0 glass gate.
//!
//! Setting `WindowBackgroundAppearance::Blurred` is only a request; this asks
//! AppKit what actually happened. gpui 0.2.2 implements the effect by adding a
//! `BlurredView` (an `NSVisualEffectView` subclass) to the window's content view
//! and making the window non-opaque, so those are the facts we assert.
//!
//! Run with `s3browser --verify-glass`.

use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::ClassType;
use objc2_app_kit::{NSApplication, NSView, NSVisualEffectView, NSWindowStyleMask};
use objc2_foundation::MainThreadMarker;

pub struct Report {
    pub lines: Vec<String>,
    pub passed: bool,
}

pub fn inspect(mtm: MainThreadMarker) -> Report {
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();

    let Some(window) = windows.iter().next() else {
        return Report {
            lines: vec!["FAIL: app has no NSWindow".into()],
            passed: false,
        };
    };

    let mut lines = Vec::new();
    let mut passed = true;
    let mut check = |ok: bool, label: &str, detail: String| {
        if !ok {
            passed = false;
        }
        lines.push(format!(
            "{} {label}: {detail}",
            if ok { "PASS" } else { "FAIL" }
        ));
    };

    // A blurred window must be non-opaque, otherwise AppKit never composites
    // anything from behind it.
    let opaque = window.isOpaque();
    check(!opaque, "window is non-opaque", format!("isOpaque={opaque}"));

    // gpui uses alpha 0.0001 rather than clearColor so the window keeps its shadow.
    let alpha = window.backgroundColor().alphaComponent();
    check(
        alpha < 0.01,
        "background is see-through",
        format!("backgroundColor.alpha={alpha}"),
    );

    let mask = window.styleMask();
    let full_size = mask.contains(NSWindowStyleMask::FullSizeContentView);
    check(
        full_size,
        "full-size content view",
        format!("styleMask contains FullSizeContentView = {full_size}"),
    );

    let transparent_titlebar = window.titlebarAppearsTransparent();
    check(
        transparent_titlebar,
        "titlebar is transparent",
        format!("titlebarAppearsTransparent={transparent_titlebar}"),
    );

    // The actual vibrancy: a real NSVisualEffectView in the hierarchy.
    let effect_views = match window.contentView() {
        Some(content) => find_effect_views(&content),
        None => Vec::new(),
    };
    check(
        !effect_views.is_empty(),
        "NSVisualEffectView installed",
        if effect_views.is_empty() {
            "none found in the content view hierarchy".to_string()
        } else {
            format!("found {}", effect_views.join(", "))
        },
    );

    Report { lines, passed }
}

/// Class names of every `NSVisualEffectView` (including subclasses such as gpui's
/// `BlurredView`) beneath `root`.
fn find_effect_views(root: &NSView) -> Vec<String> {
    let mut found = Vec::new();
    walk(root, &mut found);
    found
}

fn walk(view: &NSView, found: &mut Vec<String>) {
    if view.isKindOfClass(NSVisualEffectView::class()) {
        found.push(view.class().name().to_string_lossy().into_owned());
    }
    let subviews: Retained<_> = view.subviews();
    for subview in subviews.iter() {
        walk(&subview, found);
    }
}
