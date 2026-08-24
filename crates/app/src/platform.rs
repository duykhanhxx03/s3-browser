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

/// Whether the OS asks apps to avoid large or repeated motion.
#[cfg(target_os = "macos")]
pub fn reduce_motion() -> bool {
    use objc2_app_kit::NSWorkspace;
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

#[cfg(not(target_os = "macos"))]
pub fn reduce_motion() -> bool {
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
/// line up with our own toolbar.
///
/// `appears_transparent` is not cosmetic off macOS: on Windows GPUI reads it as
/// "hide the system caption", so the window loses its close, minimise and
/// maximise buttons unless the application draws its own. It does - the top row
/// is a `TitleBar` from gpui-component, which carries them. Linux ignores this
/// field and decides by protocol instead, and GNOME offers none, so the same
/// drawn controls are the only ones there too.
pub fn titlebar_options() -> Option<TitlebarOptions> {
    Some(TitlebarOptions {
        title: Some("s3browser".into()),
        appears_transparent: true,
        traffic_light_position: traffic_light_position(),
    })
}

/// Where macOS puts its own three buttons, or nowhere.
///
/// Nudged to line up with our toolbar normally. Parked off the window when the
/// in-app controls have been forced on, because two sets of window buttons on
/// one bar is not a preview of the platform that has one set — and gpui offers
/// no way to ask macOS to leave them out entirely.
fn traffic_light_position() -> Option<gpui::Point<gpui::Pixels>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    if window_controls_in_app() {
        return Some(point(px(-200.), px(-200.)));
    }
    Some(point(px(14.), px(13.)))
}

/// Whether the app has to draw its own close, minimise and maximise buttons.
///
/// True everywhere except macOS. The window is created with
/// `appears_transparent`, which on Windows tells the OS to hide its caption
/// entirely, and on Linux GNOME implements no server-side decoration protocol
/// at all — so on both, a window whose application draws no controls has no
/// visible way to be closed, minimised or moved. macOS keeps its traffic
/// lights floating over the transparent titlebar, and a second set beside them
/// would be two ways to close one window.
///
/// Overridable with `S3BROWSER_WINDOW_CONTROLS=0|1`, in the same spirit as
/// `S3BROWSER_GLASS` above: it is how the controls can be looked at on a Mac
/// without cross-compiling for the platform that actually needs them.
pub fn window_controls_in_app() -> bool {
    decide_window_controls(
        std::env::var("S3BROWSER_WINDOW_CONTROLS").ok().as_deref(),
        cfg!(target_os = "macos"),
    )
}

/// The decision on its own, so both branches can be tested from either
/// platform — the interesting one is by definition not the one running the test.
fn decide_window_controls(override_var: Option<&str>, native_controls: bool) -> bool {
    match override_var {
        Some("1") | Some("true") => return true,
        Some("0") | Some("false") => return false,
        _ => {}
    }
    !native_controls
}

/// Left inset of the toolbar, reserving room for the macOS traffic lights.
pub fn toolbar_leading_inset() -> f32 {
    // Room for the traffic lights only where there are traffic lights. Tied to
    // the same decision that draws our own buttons, so a forced preview does
    // not leave 88px of empty bar reserved for controls that are not there.
    if cfg!(target_os = "macos") && !window_controls_in_app() {
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
// This must match the family stored in `InterVariable.ttf`, not the product
// name used on Inter's web site. Linux's cosmic-text backend compares family
// names exactly; asking it for `Inter` leaves the embedded `Inter Variable`
// faces unused and can make text disappear when no suitable system fallback is
// installed.
pub const BUNDLED_UI_FONT: &str = "Inter Variable";

/// Kept for the case where the bundled font fails to register: these are what
/// the platform is likely to have.
#[cfg(test)]
pub fn ui_font_candidates() -> &'static [&'static str] {
    // Inter first everywhere: it is designed for screen UI at small sizes, has
    // the tall x-height that keeps a dense file list readable, and its SIL Open
    // Font Licence means shipping it later poses no licensing question.
    //
    // Then the platform's own UI font, then the safe fallbacks. The order is a
    // preference, not a promise — `pick_font` drops anything not installed.
    if cfg!(target_os = "macos") {
        &[
            "Inter Variable",
            // The real name of the macOS system font. "SF Pro Text" is what it
            // is called in design tools and is *not* installed under that name,
            // so asking for it silently fell through to gpui's default.
            ".AppleSystemUIFont",
            "Helvetica Neue",
            "Helvetica",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "Inter Variable",
            "Segoe UI Variable Text",
            "Segoe UI",
            "Arial",
        ]
    } else {
        &[
            "Inter Variable",
            "Cantarell",
            "Ubuntu",
            "DejaVu Sans",
            "Noto Sans",
        ]
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
        // Both answers are valid; what matters is getting one at all.
        let _reduced: bool = reduce_transparency();
        let _reduced_motion: bool = reduce_motion();
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
        let installed = vec!["Inter Variable".to_string(), "Helvetica".to_string()];

        // First preference wins when present.
        assert_eq!(
            pick_font(&["Inter Variable", "Helvetica"], &installed),
            "Inter Variable"
        );
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
    fn only_the_platforms_without_native_controls_draw_their_own() {
        // macOS has traffic lights floating over the transparent titlebar.
        assert!(!decide_window_controls(None, true));
        // Windows hides its caption for a transparent titlebar, and GNOME has
        // no server-side decorations to hide in the first place.
        assert!(decide_window_controls(None, false));
    }

    #[test]
    fn the_window_control_override_works_in_both_directions() {
        for value in ["1", "true"] {
            assert!(decide_window_controls(Some(value), true));
        }
        for value in ["0", "false"] {
            assert!(!decide_window_controls(Some(value), false));
        }
        // Anything else is not an override and must fall through.
        assert!(decide_window_controls(Some("maybe"), false));
    }

    #[test]
    fn font_stack_is_never_empty() {
        assert!(!ui_font_candidates().is_empty());
    }
}
