//! Colour tokens for light and dark, in glass and solid chrome.
//!
//! Only the window ground changes between chromes: in glass it is translucent so
//! the compositor's blur shows through, in solid it is opaque. Every panel above
//! it is an alpha overlay, so it composites correctly either way and there is no
//! second palette to keep in sync.

use gpui::{rgb, rgba, Hsla, WindowAppearance};

use crate::platform::Chrome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

impl Mode {
    pub fn from_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Mode::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Mode::Light,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// The window ground; translucent in glass chrome.
    pub ground: Hsla,
    /// Sidebar, toolbar and status bar.
    pub panel: Hsla,
    /// Dialogs and popovers. Opaque, unlike [`panel`]: a panel is an alpha wash
    /// meant to composite over the ground, which is right for a sidebar and
    /// wrong for something floating over content — at 5% alpha the content
    /// underneath reads straight through the dialog.
    pub modal: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    /// Highlight painted over the object list while a Finder drag is in flight.
    pub drop_target: Hsla,

    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub text_on_accent: Hsla,

    pub border: Hsla,
    pub border_strong: Hsla,

    pub accent: Hsla,
    pub danger: Hsla,

    /// The liquid-glass light model for floating surfaces.
    pub glass: GlassSpec,
}

/// How light falls on a floating pane of glass, per mode.
///
/// gpui has no per-element backdrop blur and no shader hook, so real
/// refraction is off the table — and this file's own tests forbid translucent
/// dialogs, because without a blur behind them the content underneath reads
/// straight through the text. What *can* be computed is the lighting: a pane
/// of glass under a key light from above shows a bright top rim, a soft sheen
/// down its face, a shaded thickness at its bottom edge, and it throws a deep
/// soft shadow. Each of those is one cheap layer, and every value derives from
/// one question — which way round are ground and light in this mode.
#[derive(Clone, Copy, Debug)]
pub struct GlassSpec {
    /// The hairline border. Opposite polarity to the ground: light glass on a
    /// dark ground catches light at its edge, dark-rimmed glass on a light
    /// ground reads as a cut edge.
    pub rim: Hsla,
    /// A 1px specular line inside the top edge — the key light's reflection.
    ///
    /// Zero alpha in light mode, deliberately: a white specular line on a
    /// white dialog is invisible, and painting it anyway is furniture.
    pub edge: Hsla,
    /// The face sheen, drawn as a top-down gradient to transparent.
    ///
    /// Zero in light mode, same rule as `edge`: white over a white dialog is
    /// invisible, and the one trace it *did* leave was wrong — gpui's gradient
    /// shader interpolates straight (non-premultiplied) RGBA, so white fading
    /// to transparent black passes through grey, and the "sheen" rendered as a
    /// faint darkening veil. The opposite sign of light.
    pub sheen: Hsla,
    /// The shaded thickness along the bottom inside edge, gradient upward.
    /// Always dark: glass thickness reads as shade in both modes.
    pub keel: Hsla,
    /// The drop shadow's colour. Stronger in the dark, where the panel needs
    /// separating from a ground nearly its own colour; light mode gets a
    /// fainter, wider throw because shadows on white read louder per unit of
    /// alpha.
    pub shadow: Hsla,
    /// Shadow geometry, in pixels: (y offset, blur, spread).
    pub shadow_geometry: (f32, f32, f32),
    /// How frosted the pane is: the alpha of the modal colour composited over
    /// the blurred backdrop. Below ~0.6 the content underneath fights the
    /// content on top; at 1.0 the pane is an opaque panel that paid for a
    /// capture it hides.
    pub frost: f32,
}

impl GlassSpec {
    fn new(mode: Mode) -> Self {
        match mode {
            Mode::Dark => Self {
                rim: rgba(0xffffff2b).into(),
                edge: rgba(0xffffff4d).into(),
                sheen: rgba(0xffffff12).into(),
                keel: rgba(0x00000038).into(),
                shadow: rgba(0x00000094).into(),
                shadow_geometry: (12., 36., -6.),
                frost: 0.72,
            },
            Mode::Light => Self {
                rim: rgba(0x1c202426).into(),
                edge: rgba(0xffffff00).into(),
                sheen: rgba(0xffffff00).into(),
                keel: rgba(0x0000000d).into(),
                shadow: rgba(0x0000004a).into(),
                shadow_geometry: (14., 44., -8.),
                frost: 0.78,
            },
        }
    }
}

impl Theme {
    pub fn new(mode: Mode, chrome: Chrome) -> Self {
        match mode {
            Mode::Dark => Self {
                // Near-black with a slight cool cast rather than a blue-grey:
                // a file list is mostly text, and a tinted ground tints every
                // neutral sitting on it.
                ground: if chrome.is_glass() {
                    rgba(0x0e0f12db).into()
                } else {
                    rgb(0x0e0f12).into()
                },
                panel: rgba(0xffffff0a).into(),
                modal: rgb(0x17191d).into(),
                hover: rgba(0xffffff0f).into(),
                selected: rgba(0x3b82f62e).into(),
                drop_target: rgba(0x3b82f624).into(),

                text: rgb(0xedeef0).into(),
                text_muted: rgba(0xffffffa8).into(),
                text_faint: rgba(0xffffff5c).into(),
                text_on_accent: rgb(0xffffff).into(),

                border: rgba(0xffffff14).into(),
                border_strong: rgba(0xffffff26).into(),

                // A true blue, not the indigo this used to be and not the pale
                // sky blue before that — that one was light enough next to
                // white text to read as disabled. `selected` and `drop_target`
                // are the same hue at low alpha, so a highlighted row and the
                // accent on it never look like two different blues.
                accent: rgb(0x3b82f6).into(),
                danger: rgb(0xe5484d).into(),

                glass: GlassSpec::new(Mode::Dark),
            },
            Mode::Light => Self {
                // Off-white, not pure white: a full-white ground under a full
                // white panel leaves the panel invisible.
                // Nearly opaque, unlike the dark ground's 86%. A translucent
                // light surface sits over whatever the desktop happens to be,
                // and most desktops are darker than it — at 78% the whole window
                // came out a muddy grey and only the opaque dialogs looked
                // right. Dark mode hid this because a dark ground over a dark
                // desktop is still dark.
                ground: if chrome.is_glass() {
                    rgba(0xf7f8f9f5).into()
                } else {
                    rgb(0xf7f8f9).into()
                },
                // On a light ground the panels read as *lighter*, not darker.
                panel: rgba(0xffffffb8).into(),
                modal: rgb(0xffffff).into(),
                hover: rgba(0x1c202412).into(),
                selected: rgba(0x2563eb1f).into(),
                drop_target: rgba(0x2563eb1a).into(),

                text: rgb(0x1c2024).into(),
                text_muted: rgba(0x1c2024a8).into(),
                text_faint: rgba(0x1c202470).into(),
                text_on_accent: rgb(0xffffff).into(),

                border: rgba(0x1c202418).into(),
                border_strong: rgba(0x1c20242e).into(),

                // Deeper than the dark-mode blue: the same colour that reads
                // clearly on near-black disappears into an off-white ground.
                accent: rgb(0x2563eb).into(),
                danger: rgb(0xc03a2b).into(),

                glass: GlassSpec::new(Mode::Light),
            },
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_appearance_maps_to_a_mode() {
        assert_eq!(Mode::from_appearance(WindowAppearance::Dark), Mode::Dark);
        assert_eq!(
            Mode::from_appearance(WindowAppearance::VibrantDark),
            Mode::Dark
        );
        assert_eq!(Mode::from_appearance(WindowAppearance::Light), Mode::Light);
        assert_eq!(
            Mode::from_appearance(WindowAppearance::VibrantLight),
            Mode::Light
        );
    }

    #[test]
    fn dialogs_are_opaque_in_both_modes() {
        for chrome in [Chrome::Glass, Chrome::Solid] {
            for mode in [Mode::Dark, Mode::Light] {
                let theme = Theme::new(mode, chrome);
                // A dialog floats over content. Any transparency here and the
                // list underneath reads through the text on top of it.
                assert_eq!(
                    theme.modal.a, 1.0,
                    "modal must be opaque for {chrome:?}/{mode:?}"
                );
                // The panel is deliberately not opaque; if these ever became
                // the same value the distinction would have been lost.
                assert!(theme.panel.a < 1.0);
            }
        }
    }

    #[test]
    fn a_light_ground_lets_far_less_through_than_a_dark_one() {
        // The desktop behind the window is whatever the user set, and most of
        // them are darker than an off-white panel. Dark mode can afford to be
        // see-through because dark over dark is still dark; light cannot, and
        // at the alpha dark mode uses the whole window came out muddy grey.
        let light = Theme::new(Mode::Light, Chrome::Glass).ground.a;
        let dark = Theme::new(Mode::Dark, Chrome::Glass).ground.a;
        assert!(light > dark, "light={light} dark={dark}");
        assert!(light > 0.9, "light ground is too see-through: {light}");
    }

    #[test]
    fn only_glass_chrome_leaves_the_ground_translucent() {
        for mode in [Mode::Light, Mode::Dark] {
            assert!(
                Theme::new(mode, Chrome::Glass).ground.a < 1.0,
                "{mode:?} glass ground must let the blur through"
            );
            assert_eq!(
                Theme::new(mode, Chrome::Solid).ground.a,
                1.0,
                "{mode:?} solid ground must be opaque, or the desktop shows through unblurred"
            );
        }
    }

    #[test]
    fn the_accent_is_blue_and_visible_in_both_modes() {
        // gpui keeps hue as 0..1; blue runs from roughly 200° to 250°.
        for mode in [Mode::Light, Mode::Dark] {
            let theme = Theme::new(mode, Chrome::Solid);
            assert!(
                (0.55..=0.70).contains(&theme.accent.h),
                "{mode:?} accent is not blue: h={}",
                theme.accent.h
            );
            // Visible against its own ground, which is the whole job: an accent
            // that matches the ground's lightness is a colour nobody can see.
            assert!(
                (theme.accent.l - theme.ground.l).abs() > 0.2,
                "{mode:?} accent does not stand off its ground"
            );
            // The row highlight is the same hue, so a selected row and the
            // folder icon on it are one colour rather than two blues arguing.
            assert!((theme.selected.h - theme.accent.h).abs() < 0.02);
        }
    }

    #[test]
    fn glass_light_falls_from_above_and_actually_paints() {
        let dark = GlassSpec::new(Mode::Dark);
        let light = GlassSpec::new(Mode::Light);

        for (mode, glass) in [("dark", dark), ("light", light)] {
            // The sheen is painted *under* the content, so this cap is about
            // the panel's own face: past it the face reads tinted rather than
            // lit, and the layer stops being light on glass.
            assert!(glass.sheen.a <= 0.12, "{mode} sheen too strong");
            // Thickness reads as shade in both modes — a bright keel would
            // mean the light comes from below, which nothing else agrees with.
            assert_eq!(glass.keel.l, 0.0, "{mode} keel must be dark");
            // And it must actually paint: rgba(0,0,0,0) has l == 0 too, and a
            // spec of zeroes would pass every cap in this test while drawing
            // no glass at all. The floors are what make the model real.
            assert!(glass.keel.a > 0.03, "{mode} keel must be visible");
            assert!(glass.rim.a > 0.05, "{mode} rim must be visible");

            let (y, blur, spread) = glass.shadow_geometry;
            assert!(y > 0., "{mode} shadow must fall downward");
            assert!(blur > 0., "{mode} shadow must be soft, not a hard offset");
            // Negative spread keeps the blur from leaking past the corners as
            // a visible halo ring.
            assert!(spread < 0., "{mode} shadow spread should be negative");
        }

        // The rim is the opposite polarity of the ground: an edge catching
        // light on near-black, a cut edge on white.
        assert!(dark.rim.l > 0.9, "dark rim catches light");
        assert!(light.rim.l < 0.2, "light rim reads as a cut edge");

        // The specular layers only exist where they can be seen. White on a
        // white dialog is invisible, and painting it anyway is furniture — and
        // in the sheen's case it was worse than furniture: gpui interpolates
        // gradients in straight alpha, so white-to-transparent-black passes
        // through grey and the layer rendered as a *darkening* veil.
        assert!(dark.edge.a > 0.2);
        assert!(dark.sheen.a > 0.04, "dark sheen is the one that must paint");
        assert_eq!(light.edge.a, 0.0);
        assert_eq!(light.sheen.a, 0.0);

        // Dark needs the stronger shadow — the panel is nearly the ground's
        // own colour; light gets a wider, fainter throw instead.
        assert!(dark.shadow.a > light.shadow.a);
        assert!(light.shadow_geometry.1 > dark.shadow_geometry.1);

        // Frost has a floor and a ceiling. Below it the backdrop fights the
        // dialog's own text; at 1.0 the pane is opaque and the whole capture
        // was for nothing. Light frosts harder because dark text needs a
        // steadier ground than light text does.
        for (mode, glass) in [("dark", dark), ("light", light)] {
            assert!(glass.frost >= 0.6 && glass.frost < 1.0, "{mode} frost");
        }
        assert!(light.frost > dark.frost);
    }

    #[test]
    fn text_contrasts_with_its_ground_in_both_modes() {
        // Rough luminance check: dark mode paints light text, light mode dark text.
        let dark = Theme::new(Mode::Dark, Chrome::Solid);
        let light = Theme::new(Mode::Light, Chrome::Solid);
        assert!(dark.text.l > dark.ground.l, "dark mode needs light text");
        assert!(light.text.l < light.ground.l, "light mode needs dark text");
    }
}
