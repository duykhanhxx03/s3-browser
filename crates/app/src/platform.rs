//! Every macOS/Windows/Linux difference the UI cares about, in one place.
//!
//! GPUI runs on all three, but the window chrome does not behave the same way:
//! blur is real vibrancy on macOS, Acrylic on Windows, and only exists on Linux
//! under KWin. Rather than let a translucent theme render unreadable on a
//! compositor that ignores the request, the app picks a [`Chrome`] mode up front
//! and the theme paints solid surfaces whenever blur is not expected to work.

use gpui::{point, px, TitlebarOptions, WindowBackgroundAppearance};

/// Whether this platform's compositor can blur a window at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformBlur {
    Available,
    Unavailable,
}

impl PlatformBlur {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            // NSVisualEffectView vibrancy, supported since macOS 12.
            PlatformBlur::Available
        } else if cfg!(target_os = "windows") {
            // Acrylic via SetWindowCompositionAttribute.
            PlatformBlur::Available
        } else {
            // Wayland blur exists only through KDE's org_kde_kwin_blur protocol,
            // and X11 has no blur control at all, so solid is the honest default.
            PlatformBlur::Unavailable
        }
    }
}

/// Whether the OS asks apps to avoid translucency.
///
/// Only macOS is queried: Windows exposes a similar setting but gpui's Acrylic
/// path does not read it, and on Linux the default is already solid.
#[cfg(target_os = "macos")]
pub fn reduce_transparency() -> bool {
    use objc2_app_kit::NSWorkspace;
    // Reading AppKit state needs the main thread; `detect` is called during
    // window setup, which is on it.
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceTransparency()
}

#[cfg(not(target_os = "macos"))]
pub fn reduce_transparency() -> bool {
    false
}

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
        Chrome::decide(
            std::env::var("S3BROWSER_GLASS").ok().as_deref(),
            reduce_transparency(),
            PlatformBlur::current(),
        )
    }

    /// The decision itself, separated from reading the environment and AppKit so
    /// every branch can be tested — including "reduce transparency is on", which
    /// otherwise would mean changing a system accessibility setting to check.
    fn decide(override_var: Option<&str>, reduce_transparency: bool, blur: PlatformBlur) -> Self {
        // An explicit override wins over everything, including the accessibility
        // setting: someone who sets this is overriding deliberately.
        match override_var {
            Some("1") | Some("true") => return Chrome::Glass,
            Some("0") | Some("false") => return Chrome::Solid,
            _ => {}
        }

        // "Reduce transparency" is switched on by people who find translucency
        // hard to read. Blur is exactly what they asked to be rid of, so this
        // outranks looking nice.
        if reduce_transparency {
            return Chrome::Solid;
        }

        match blur {
            PlatformBlur::Available => Chrome::Glass,
            PlatformBlur::Unavailable => Chrome::Solid,
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
/// The family name of the font compiled into the binary.
///
/// Used directly rather than looked up: a bundled font is registered with the
/// text system at startup and is therefore always present, while
/// `all_font_names` reports only what the operating system has installed.
/// Probing for it finds nothing and falls through to a system font, discarding
/// the very font that was bundled to avoid that.
pub const BUNDLED_UI_FONT: &str = "Inter";

/// Kept for the case where the bundled font fails to register: these are what
/// the platform is likely to have.
pub fn ui_font_candidates() -> &'static [&'static str] {
    // Inter first everywhere: it is designed for screen UI at small sizes, has
    // the tall x-height that keeps a dense file list readable, and its SIL Open
    // Font Licence means shipping it later poses no licensing question.
    //
    // Then the platform's own UI font, then the safe fallbacks. The order is a
    // preference, not a promise — `pick_font` drops anything not installed.
    if cfg!(target_os = "macos") {
        &[
            "Inter",
            // The real name of the macOS system font. "SF Pro Text" is what it
            // is called in design tools and is *not* installed under that name,
            // so asking for it silently fell through to gpui's default.
            ".AppleSystemUIFont",
            "Helvetica Neue",
            "Helvetica",
        ]
    } else if cfg!(target_os = "windows") {
        &["Inter", "Segoe UI Variable Text", "Segoe UI", "Arial"]
    } else {
        &["Inter", "Cantarell", "Ubuntu", "DejaVu Sans", "Noto Sans"]
    }
}

/// The first candidate the system actually has.
///
/// Naming a font that is not installed is silent: the text still renders, in
/// whatever the toolkit falls back to, so the app can spend its whole life in a
/// font nobody chose. Checking turns that into a decision.
pub fn pick_font(candidates: &[&str], available: &[String]) -> String {
    candidates
        .iter()
        .find(|candidate| {
            available
                .iter()
                .any(|font| font.eq_ignore_ascii_case(candidate))
        })
        .map(|found| (*found).to_string())
        // Nothing matched: hand back the first preference and let the toolkit
        // fall back, which is still better than an empty family name.
        .unwrap_or_else(|| candidates.first().copied().unwrap_or("sans-serif").into())
}

/// Monospace for keys, checksums and previews, where alignment carries meaning.
pub fn mono_font_candidates() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["SF Mono", "Menlo", "Monaco", "Courier New"]
    } else if cfg!(target_os = "windows") {
        &["Cascadia Mono", "Consolas", "Courier New"]
    } else {
        &["JetBrains Mono", "DejaVu Sans Mono", "monospace"]
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
        assert_eq!(
            Chrome::decide(None, false, PlatformBlur::Unavailable),
            Chrome::Solid
        );
        assert_eq!(
            Chrome::decide(None, false, PlatformBlur::Available),
            Chrome::Glass
        );
    }

    /// The pure tests above cover the decision; this covers the part they cannot
    /// — that the AppKit call is bound correctly and returns without crashing.
    /// Asserting a specific value would fail on a machine where the setting is
    /// on, which is a legitimate way to run.
    #[test]
    fn reading_the_accessibility_setting_does_not_crash() {
        let reduced = reduce_transparency();
        // Both answers are valid; what matters is getting one at all.
        assert!(reduced == true || reduced == false);
    }

    #[test]
    fn reduce_transparency_wins_over_a_platform_that_can_blur() {
        // The whole point: someone who turned this on did so because
        // translucency is hard for them to read.
        assert_eq!(
            Chrome::decide(None, true, PlatformBlur::Available),
            Chrome::Solid
        );
        assert_eq!(
            Chrome::decide(None, true, PlatformBlur::Unavailable),
            Chrome::Solid
        );
    }

    #[test]
    fn an_explicit_override_outranks_both() {
        // Someone setting the variable is overriding on purpose, including
        // back on despite the accessibility setting.
        for value in ["1", "true"] {
            assert_eq!(
                Chrome::decide(Some(value), true, PlatformBlur::Unavailable),
                Chrome::Glass
            );
        }
        for value in ["0", "false"] {
            assert_eq!(
                Chrome::decide(Some(value), false, PlatformBlur::Available),
                Chrome::Solid
            );
        }
        // Anything unrecognised is not an override and must fall through.
        assert_eq!(
            Chrome::decide(Some("maybe"), false, PlatformBlur::Available),
            Chrome::Glass
        );
    }

    #[test]
    fn the_chosen_font_is_one_the_system_has() {
        let installed = vec!["Inter".to_string(), "Helvetica".to_string()];

        // First preference wins when present.
        assert_eq!(pick_font(&["Inter", "Helvetica"], &installed), "Inter");
        // Missing preferences are skipped rather than requested and ignored —
        // the bug this exists for: the app asked for "SF Pro Text", which is
        // not installed under that name, and silently rendered in something
        // else for its whole life.
        assert_eq!(
            pick_font(&["SF Pro Text", "Helvetica"], &installed),
            "Helvetica"
        );
        // Font names come back from the OS in whatever case it likes.
        assert_eq!(pick_font(&["inter"], &installed), "inter");

        // Nothing available: still return something nameable rather than an
        // empty family, which renders as a blank run of text.
        assert_eq!(pick_font(&["Nope", "Nada"], &installed), "Nope");
        assert_eq!(pick_font(&[], &installed), "sans-serif");
    }

    #[test]
    fn font_stack_is_never_empty() {
        assert!(!ui_font_candidates().is_empty());
    }
}
