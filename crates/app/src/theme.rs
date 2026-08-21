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

    /// A control's face: the top and bottom of a vertical gradient, plus its
    /// hairline. One material for every button and chip in the app — a flat
    /// grey wash was what buttons had before, and flat fills on glass panes
    /// read as stickers. The gradient is the curvature: lighter where a convex
    /// cap faces the key light, darker where it turns away.
    pub control_top: Hsla,
    pub control_bottom: Hsla,
    pub control_border: Hsla,
}

/// How light falls on a floating pane of glass, per mode.
///
/// The pane itself — capture, blur, refraction, edge lighting — lives in the
/// forked renderer's glass shader, computed on the rounded-rect SDF. What
/// stays here is only what a shader cannot own: the hairline rim, the drop
/// shadow, and how frosted the pane is. Every value still derives from one
/// question — which way round are ground and light in this mode.
#[derive(Clone, Copy, Debug)]
pub struct GlassSpec {
    /// The hairline border. Opposite polarity to the ground: light glass on a
    /// dark ground catches light at its edge, dark-rimmed glass on a light
    /// ground reads as a cut edge.
    pub rim: Hsla,
    /// The drop shadow's colour. Stronger in the dark, where the panel needs
    /// separating from a ground nearly its own colour; light mode gets a
    /// fainter, wider throw because shadows on white read louder per unit of
    /// alpha.
    pub shadow: Hsla,
    /// Shadow geometry, in pixels: (y offset, blur, spread).
    pub shadow_geometry: (f32, f32, f32),
    /// How frosted the pane is: the alpha of the modal colour composited over
    /// the blurred backdrop. Liquid glass is *thin* frost — the first cut used
    /// 0.72/0.78 and read as frosted plastic, not glass; what carries text
    /// legibility at low frost is the blur underneath, not the tint. At 1.0
    /// the pane is an opaque panel that paid for a capture it hides.
    pub frost: f32,
}

impl GlassSpec {
    fn new(mode: Mode) -> Self {
        match mode {
            Mode::Dark => Self {
                rim: rgba(0xffffff2b).into(),
                shadow: rgba(0x00000094).into(),
                shadow_geometry: (12., 36., -6.),
                // The reverse-engineered tables put dark panes at 0.40 —
                // *more* opaque than light, the reverse of every earlier guess
                // here: dark mode leans on tint for legibility where light
                // mode can lean on a bright blur.
                frost: 0.40,
            },
            Mode::Light => Self {
                rim: rgba(0x1c202426).into(),
                shadow: rgba(0x0000004a).into(),
                shadow_geometry: (14., 44., -8.),
                frost: 0.25,
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

                control_top: rgba(0xffffff1f).into(),
                control_bottom: rgba(0xffffff0d).into(),
                control_border: rgba(0xffffff21).into(),
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

                // Light mode caps are white glass over a light ground: mostly
                // white, with enough alpha drop at the bottom to read convex.
                control_top: rgba(0xffffffe0).into(),
                control_bottom: rgba(0xffffff8c).into(),
                control_border: rgba(0x1c202421).into(),
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
    fn controls_are_convex_caps_under_the_same_light() {
        // The gradient is the curvature: the top of a raised cap faces the key
        // light, so it must be the brighter end in both modes — a gradient the
        // other way up reads as a well, not a button.
        for mode in [Mode::Dark, Mode::Light] {
            let theme = Theme::new(mode, Chrome::Glass);
            let top = (theme.control_top.l, theme.control_top.a);
            let bottom = (theme.control_bottom.l, theme.control_bottom.a);
            // Brighter means more light reaches the eye: same lightness at
            // higher alpha, or higher lightness outright.
            assert!(
                top.0 > bottom.0 || (top.0 == bottom.0 && top.1 > bottom.1),
                "{mode:?} control cap is lit from below"
            );
            assert!(theme.control_border.a > 0.05, "{mode:?} rim must paint");
        }
    }

    #[test]
    fn glass_is_lit_from_above_and_stays_glass() {
        let dark = GlassSpec::new(Mode::Dark);
        let light = GlassSpec::new(Mode::Light);

        for (mode, glass) in [("dark", dark), ("light", light)] {
            // The rim must actually paint: a spec of zeroes would pass every
            // cap in this test while drawing no glass at all.
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

        // Dark needs the stronger shadow — the panel is nearly the ground's
        // own colour; light gets a wider, fainter throw instead.
        assert!(dark.shadow.a > light.shadow.a);
        assert!(light.shadow_geometry.1 > dark.shadow_geometry.1);

        // Frost has a floor and a ceiling. Below the floor the backdrop
        // fights the dialog's own text even through the blur; past the
        // ceiling the pane is frosted plastic and the whole capture was for
        // nothing — which is exactly what the first cut at 0.72/0.78 looked
        // like. Light frosts harder because dark text needs a steadier ground.
        for (mode, glass) in [("dark", dark), ("light", light)] {
            assert!(glass.frost >= 0.2 && glass.frost <= 0.45, "{mode} frost");
        }
        // Dark frosts harder than light — the reverse of this test's first
        // guess. The reverse-engineered material tables put dark panes at
        // 0.40 and light at 0.25: a dark pane cannot brighten its backdrop
        // into legibility, so it has to tint it away instead.
        assert!(dark.frost > light.frost);
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
