//! Every macOS/Windows/Linux difference the UI cares about, in one place.
//!
//! GPUI runs on all three, but the window chrome does not behave the same way:
//! blur is real vibrancy on macOS, Acrylic on Windows, and only exists on Linux
//! under KWin. Rather than let a translucent theme render unreadable on a
//! compositor that ignores the request, the app picks a [`Chrome`] mode up front
//! and the theme paints solid surfaces whenever blur is not expected to work.

use gpui::{point, px, TitlebarOptions, WindowBackgroundAppearance};

/// How the window is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chrome {
    /// Translucent surfaces over a blurred backdrop.
    Glass,
    /// Opaque surfaces; what we use when the compositor will not blur.
    Solid,
}

impl Chrome {
    /// The default for this platform, overridable with `S3BROWSER_GLASS=0|1` so
    /// a KDE user can opt in and anyone can opt out.
    pub fn detect() -> Self {
        match std::env::var("S3BROWSER_GLASS").as_deref() {
            Ok("1") | Ok("true") => return Chrome::Glass,
            Ok("0") | Ok("false") => return Chrome::Solid,
            _ => {}
        }

        if cfg!(target_os = "macos") {
            // NSVisualEffectView vibrancy, supported since macOS 12.
            Chrome::Glass
        } else if cfg!(target_os = "windows") {
            // Acrylic via SetWindowCompositionAttribute.
            Chrome::Glass
        } else {
            // Wayland blur exists only through KDE's org_kde_kwin_blur protocol,
            // and X11 has no blur control at all, so solid is the honest default.
            Chrome::Solid
        }
    }

    pub fn is_glass(self) -> bool {
        self == Chrome::Glass
    }

    pub fn window_background(self) -> WindowBackgroundAppearance {
        match self {
            Chrome::Glass => WindowBackgroundAppearance::Blurred,
            Chrome::Solid => WindowBackgroundAppearance::Opaque,
        }
    }
}

/// Titlebar setup. On macOS the traffic lights stay native and are nudged to
/// line up with our own toolbar; elsewhere GPUI ignores the position and the
/// window keeps its system controls.
pub fn titlebar_options() -> Option<TitlebarOptions> {
    Some(TitlebarOptions {
        title: Some("s3browser".into()),
        appears_transparent: true,
        traffic_light_position: if cfg!(target_os = "macos") {
            Some(point(px(14.), px(13.)))
        } else {
            None
        },
    })
}

/// Left inset of the toolbar, reserving room for the macOS traffic lights.
pub fn toolbar_leading_inset() -> f32 {
    if cfg!(target_os = "macos") {
        88.
    } else {
        12.
    }
}

/// UI font stack. GPUI falls back through the list, so naming each platform's
/// system face keeps text native-looking everywhere.
pub fn ui_font_candidates() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["SF Pro Text", "Helvetica Neue", "Helvetica"]
    } else if cfg!(target_os = "windows") {
        &["Segoe UI Variable Text", "Segoe UI", "Arial"]
    } else {
        &["Inter", "Cantarell", "Ubuntu", "DejaVu Sans", "Noto Sans"]
    }
}

/// Label for the platform's primary modifier, used in menu hints.
pub fn primary_modifier() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_maps_to_a_blurred_window_and_solid_to_opaque() {
        assert_eq!(
            Chrome::Glass.window_background(),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            Chrome::Solid.window_background(),
            WindowBackgroundAppearance::Opaque
        );
    }

    #[test]
    fn linux_defaults_to_solid_because_blur_is_kde_only() {
        // `detect` reads the environment, so assert the platform rule directly.
        let expected = if cfg!(any(target_os = "macos", target_os = "windows")) {
            Chrome::Glass
        } else {
            Chrome::Solid
        };
        // Guard against an env override leaking in from the developer's shell.
        if std::env::var("S3BROWSER_GLASS").is_err() {
            assert_eq!(Chrome::detect(), expected);
        }
    }

    #[test]
    fn font_stack_is_never_empty() {
        assert!(!ui_font_candidates().is_empty());
    }
}
