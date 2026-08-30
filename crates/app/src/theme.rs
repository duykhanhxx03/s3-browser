//! Colour tokens for light and dark, in glass and solid chrome.
//!
//! The window ground changes between chromes: in glass it is translucent so the
//! compositor's blur shows through, in solid it is opaque. The workbench
//! surfaces above it are alpha overlays, so title bars, sidebars and data panes
//! keep one palette while still feeling layered.

use gpui::{rgb, rgba, Hsla, WindowAppearance};

use crate::color;
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
    /// Heavier window chrome: title bar, sidebar and docked utility panes.
    pub chrome: Hsla,
    /// Stronger chrome for strips that need to sit above data, such as tabs
    /// and table headers.
    pub chrome_strong: Hsla,
    /// The data canvas between the chrome surfaces.
    pub workspace: Hsla,
    /// Raised but still in-window surfaces: inspector sections, grid media
    /// wells and compact controls.
    pub surface_high: Hsla,
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
                chrome: rgba(0xffffff10).into(),
                chrome_strong: rgba(0xffffff17).into(),
                workspace: rgba(0xffffff05).into(),
                surface_high: rgba(0xffffff13).into(),
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
                chrome: rgba(0xffffffcf).into(),
                chrome_strong: rgba(0xffffffeb).into(),
                workspace: rgba(0xffffff78).into(),
                surface_high: rgba(0xffffffea).into(),
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
            },
        }
    }

    /// Builds the whole theme out of a palette.
    ///
    /// Not a tint over the built-in one, which is what this used to be: the
    /// palette was blended in at a tenth alpha, so a chosen palette moved the
    /// window ground by about five parts in 441 and, for one of them, by
    /// nothing measurable at all. These four colours *are* the scheme, so the
    /// scheme is built from them.
    ///
    /// Every derived colour is then checked against the surface it will sit
    /// on and adjusted until it is legible — see [`crate::color`] for why the
    /// old `Hsla::l` comparisons could not do that.
    pub fn from_palette(
        colors: [u32; 4],
        accent_hint: Option<u32>,
        mode: Mode,
        chrome: Chrome,
    ) -> Self {
        let dark = mode == Mode::Dark;

        let mut ordered: Vec<Hsla> = colors.map(|hex| Hsla::from(rgb(hex))).to_vec();
        ordered.sort_by(|a, b| color::luminance(*a).total_cmp(&color::luminance(*b)));

        // How much colour a surface may carry depends on how much of the
        // window it covers. A palette is chosen as four small swatches, but
        // the ground is a whole window of one of them, and a chroma that gives
        // a chip its character glares across 1120x720. Material 3 draws its
        // surfaces from a neutral at 0.02 chroma and its borders from a
        // variant at 0.04, which is the scale used here. The accent is left
        // alone: a small area is exactly where a palette should be loud.
        const GROUND_CHROMA: f32 = 0.02;
        const SURFACE_CHROMA: f32 = 0.03;
        const LINE_CHROMA: f32 = 0.045;
        const TEXT_CHROMA: f32 = 0.06;

        // Capped before anything is measured against it, so every contrast
        // check below sees the colour that will actually be painted.
        let ground = color::cap_chroma(
            if dark {
                ordered[0]
            } else {
                ordered[ordered.len() - 1]
            },
            GROUND_CHROMA,
        );
        let ground_l = color::lightness(ground);

        // Text is the palette's own far end when that end is readable, and
        // the same hue pushed until it is when it is not. Aimed past AA at
        // 7:1, because body text is what the whole window is made of.
        // Capped first and lifted second, so readability has the last word.
        let far = color::cap_chroma(
            if dark {
                ordered[ordered.len() - 1]
            } else {
                ordered[0]
            },
            TEXT_CHROMA,
        );
        let text = color::lift_until(far, ground, 7.0, if dark { 1.0 } else { 0.0 });

        // Surfaces step away from the ground in perceptual lightness, so the
        // ramp looks even whatever hue the palette happens to be — and each
        // step is pulled back if it drifts far enough to cost the body text
        // its 4.5:1.
        let step = if dark { 0.055 } else { -0.045 };
        let near = color::cap_chroma(
            if dark {
                ordered[1]
            } else {
                ordered[ordered.len() - 2]
            },
            SURFACE_CHROMA,
        );
        let surface = |seed: Hsla, want: f32| -> Hsla {
            for i in (1..=12).rev() {
                let candidate =
                    color::with_lightness(seed, ground_l + step * want * (i as f32 / 12.));
                if color::contrast(text, candidate) >= color::CONTRAST_TEXT {
                    return candidate;
                }
            }
            ground
        };

        // The accent is the most colourful of the four that is neither the
        // ground nor the text, lifted only as far as 3:1 demands.
        let accent = accent_hint
            .map(|hex| Hsla::from(rgb(hex)))
            .unwrap_or_else(|| {
                ordered
                    .iter()
                    .copied()
                    .filter(|c| *c != ground && *c != far)
                    .chain(ordered.iter().copied().filter(|c| *c != ground))
                    .max_by(|a, b| color::chroma(*a).total_cmp(&color::chroma(*b)))
                    .unwrap_or(ordered[ordered.len() / 2])
            });
        let white: Hsla = rgb(0xffffff).into();
        let black: Hsla = rgb(0x1c2024).into();

        // The accent has two jobs at once: stand off the ground it sits on,
        // and carry a label. A mid-lightness accent can clear the first and
        // still fail the second — Lavender Night's `#6f7fc8` reached 3:1 on
        // its ground while leaving both black and white under 4.5:1 on top of
        // it. Both constraints pull the same way, away from the ground, so
        // one walk satisfies them together.
        let limit = if dark { 1.0 } else { 0.0 };
        let accent = {
            let start = color::lightness(accent);
            let mut chosen = accent;
            for step in 0..=40 {
                chosen =
                    color::with_lightness(accent, start + (limit - start) * (step as f32 / 40.));
                let stands_out = color::contrast(chosen, ground) >= color::CONTRAST_UI;
                let carries_a_label = color::contrast(white, chosen)
                    .max(color::contrast(black, chosen))
                    >= color::CONTRAST_TEXT;
                if stands_out && carries_a_label {
                    break;
                }
            }
            chosen
        };

        let panel = surface(near, 1.0).alpha(if dark { 0.72 } else { 0.78 });
        let chrome_surface = surface(near, 1.25).alpha(if dark { 0.78 } else { 0.86 });
        let chrome_strong = surface(near, 1.55).alpha(if dark { 0.86 } else { 0.94 });
        let workspace = surface(ground, 0.35).alpha(if dark { 0.34 } else { 0.42 });
        let surface_high = surface(near, 1.45).alpha(if dark { 0.88 } else { 0.96 });

        Self {
            // Glass keeps the palette's ground but lets the compositor
            // through, exactly as the built-in theme does.
            ground: if chrome.is_glass() {
                ground.alpha(if dark { 0.86 } else { 0.96 })
            } else {
                ground
            },
            panel,
            chrome: chrome_surface,
            chrome_strong,
            workspace,
            surface_high,
            modal: surface(near, 1.7),
            hover: surface(ground, 2.1),
            selected: accent.alpha(if dark { 0.24 } else { 0.20 }),
            drop_target: accent.alpha(if dark { 0.18 } else { 0.15 }),

            text,
            // Measured down to a target rather than stepped down by a fixed
            // amount, so neither tier can quietly fall under its minimum.
            text_muted: color::fade_toward(text, ground, 5.5),
            text_faint: color::fade_toward(text, ground, 3.4),
            text_on_accent: if color::contrast(white, accent) >= color::contrast(black, accent) {
                white
            } else {
                black
            },

            border: color::cap_chroma(
                color::with_lightness(ground, ground_l + step * 3.0),
                LINE_CHROMA,
            ),
            border_strong: color::cap_chroma(
                color::with_lightness(ground, ground_l + step * 4.4),
                LINE_CHROMA,
            ),

            accent,
            danger: color::lift_until(
                rgb(if dark { 0xff6b6b } else { 0xc03a2b }).into(),
                ground,
                color::CONTRAST_UI,
                limit,
            ),
        }
    }

    /// The two wordmark colours, for a wordmark drawn at `font_size`.
    ///
    /// Lives on the theme rather than in the logo so the test exercises the
    /// same code the title bar does — a test that recomputed the rule would go
    /// on passing whatever the logo actually did.
    pub fn wordmark(self, font_size: f32) -> (Hsla, Hsla) {
        // WCAG counts bold text from 18.66px up as large, which needs only
        // 3:1. The title bar draws this at 15px and does not qualify.
        let needed = if font_size >= 18.66 {
            color::CONTRAST_UI
        } else {
            color::CONTRAST_TEXT
        };
        let ground = self.ground.alpha(1.0);
        let toward = if color::luminance(ground) < 0.5 {
            1.0
        } else {
            0.0
        };
        (
            color::lift_until(self.accent, ground, needed, toward),
            self.text,
        )
    }

    /// Which side of light and dark this theme actually landed on.
    ///
    /// Read off the ground, not off the setting that asked for it: a palette
    /// that cannot be built in the mode requested overrides it, and anything
    /// still consulting the setting then disagrees with the window.
    pub fn mode(self) -> Mode {
        if color::luminance(self.ground.alpha(1.0)) < 0.5 {
            Mode::Dark
        } else {
            Mode::Light
        }
    }

    /// Which modes a palette can actually be built in.
    ///
    /// Not a matter of taste: a dark theme needs one of the four to be dark
    /// enough to carry light text, and a light theme needs one light enough to
    /// carry dark text. A pastel set has no dark end at all, and no amount of
    /// setting can give it one — the honest answer is that it is a light
    /// scheme, not that its dark mode is disappointing.
    pub fn palette_modes(colors: [u32; 4]) -> (bool, bool) {
        /// Dark enough to be a ground under light text.
        const DARK_MAX: f32 = 0.15;
        /// Light enough to be a ground under dark text.
        const LIGHT_MIN: f32 = 0.55;

        let mut lums: Vec<f32> = colors
            .iter()
            .map(|hex| color::luminance(Hsla::from(rgb(*hex))))
            .collect();
        lums.sort_by(|a, b| a.total_cmp(b));
        (lums[0] <= DARK_MAX, lums[lums.len() - 1] >= LIGHT_MIN)
    }

    /// The mode a palette will be painted in, given what the user asked for.
    ///
    /// The preference is honoured wherever the palette can do both, which is
    /// about a third of them — so the light/dark setting keeps working rather
    /// than being switched off the moment a palette is chosen.
    pub fn palette_mode(colors: [u32; 4], preferred: Mode) -> Mode {
        match Self::palette_modes(colors) {
            (true, true) => preferred,
            (true, false) => Mode::Dark,
            (false, true) => Mode::Light,
            // Neither end is usable on its own; go with the half it is
            // closer to and let the derivation push a readable text colour.
            (false, false) => preferred,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ColorPalette;

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

    /// Every colour a palette produces, checked against the surface it is
    /// actually drawn on.
    ///
    /// This is the test that would have caught the bug this rewrite fixes:
    /// Charcoal Teal used to paint white on its `#00adb5` accent at 2.75:1,
    /// under even the 3:1 floor for large text, because the decision was made
    /// by comparing `Hsla::l` against a constant.
    #[test]
    fn every_palette_is_legible_on_every_surface_it_paints() {
        for palette in ColorPalette::ALL {
            let Some(accent) = palette.accent() else {
                continue; // the default palette is not derived
            };
            let (can_dark, can_light) = Theme::palette_modes(palette.colors());
            let modes: Vec<Mode> = [(Mode::Dark, can_dark), (Mode::Light, can_light)]
                .into_iter()
                .filter(|(_, ok)| *ok)
                .map(|(m, _)| m)
                .collect();
            assert!(
                !modes.is_empty(),
                "{palette:?} can be built in no mode at all"
            );

            for chrome in [Chrome::Solid, Chrome::Glass] {
                for mode in modes.iter().copied() {
                    let theme = Theme::from_palette(palette.colors(), Some(accent), mode, chrome);
                    let ground = theme.ground.alpha(1.0);

                    for (name, surface) in [
                        ("ground", ground),
                        ("panel", theme.panel),
                        ("chrome", theme.chrome),
                        ("chrome_strong", theme.chrome_strong),
                        ("workspace", theme.workspace),
                        ("surface_high", theme.surface_high),
                        ("modal", theme.modal),
                        ("hover", theme.hover),
                    ] {
                        assert!(
                            color::contrast(theme.text, surface) >= color::CONTRAST_TEXT,
                            "{palette:?}/{chrome:?}: body text on {name} is {:.2}:1",
                            color::contrast(theme.text, surface)
                        );
                    }
                    assert!(
                        color::contrast(theme.text_muted, ground) >= color::CONTRAST_TEXT,
                        "{palette:?}: muted text is {:.2}:1",
                        color::contrast(theme.text_muted, ground)
                    );
                    assert!(
                        color::contrast(theme.text_faint, ground) >= color::CONTRAST_UI,
                        "{palette:?}: faint text is {:.2}:1",
                        color::contrast(theme.text_faint, ground)
                    );
                    assert!(
                        color::contrast(theme.accent, ground) >= color::CONTRAST_UI,
                        "{palette:?}: accent is {:.2}:1 on its ground",
                        color::contrast(theme.accent, ground)
                    );
                    assert!(
                        color::contrast(theme.text_on_accent, theme.accent) >= color::CONTRAST_TEXT,
                        "{palette:?}: text on the accent is {:.2}:1",
                        color::contrast(theme.text_on_accent, theme.accent)
                    );
                    assert!(
                        color::contrast(theme.danger, ground) >= color::CONTRAST_UI,
                        "{palette:?}: danger is {:.2}:1 on its ground",
                        color::contrast(theme.danger, ground)
                    );
                }
            }
        }
    }

    /// The point of the rewrite: the window's ground comes from the palette,
    /// not from the built-in theme with a wash of palette over it.
    ///
    /// Not an exact match any more — the ground's chroma is capped so a
    /// saturated palette does not glare across the whole window — so what is
    /// checked is that it is one of the palette's colours with only its
    /// colourfulness taken down: same lightness, never more chroma than it
    /// started with, and nowhere near the stock ground it used to be.
    #[test]
    fn a_palette_actually_paints_its_own_ground() {
        for palette in ColorPalette::ALL {
            let Some(accent) = palette.accent() else {
                continue;
            };
            let mode = Theme::palette_mode(palette.colors(), Mode::Dark);
            let theme = Theme::from_palette(palette.colors(), Some(accent), mode, Chrome::Solid);
            let ground = theme.ground;

            let from_palette = palette.colors().iter().any(|hex| {
                let source: Hsla = rgb(*hex).into();
                (color::lightness(source) - color::lightness(ground)).abs() < 0.01
                    && color::chroma(ground) <= color::chroma(source) + 0.001
            });
            assert!(
                from_palette,
                "{palette:?} ground {ground:?} did not come from the palette"
            );

            let stock = Theme::new(mode, Chrome::Solid).ground;
            assert!(
                color::contrast(ground, stock) > 1.02 || color::chroma(ground) > 0.,
                "{palette:?} ground is indistinguishable from the built-in one"
            );
        }
    }

    /// Nothing large is allowed to be vivid.
    ///
    /// The derivation used to hand a palette's own colour straight to the
    /// ground and the panel, which is right for a swatch and wrong for a
    /// window: Aqua Punch painted its sidebar `#4cf8f4`, a chroma of 0.137
    /// where the built-in dark ground sits at 0.006. Across 120 real Color
    /// Hunt palettes, 63% of panels came out above 0.04.
    #[test]
    fn no_large_surface_is_vivid_enough_to_glare() {
        // Material 3's neutral, and the variant it draws borders from.
        const SURFACE_MAX: f32 = 0.035;
        const LINE_MAX: f32 = 0.05;

        for palette in ColorPalette::ALL {
            let Some(accent) = palette.accent() else {
                continue;
            };
            let (can_dark, can_light) = Theme::palette_modes(palette.colors());
            for (mode, ok) in [(Mode::Dark, can_dark), (Mode::Light, can_light)] {
                if !ok {
                    continue;
                }
                let theme =
                    Theme::from_palette(palette.colors(), Some(accent), mode, Chrome::Solid);
                for (name, surface, limit) in [
                    ("ground", theme.ground.alpha(1.0), SURFACE_MAX),
                    ("panel", theme.panel, SURFACE_MAX),
                    ("modal", theme.modal, SURFACE_MAX),
                    ("hover", theme.hover, SURFACE_MAX),
                    ("border", theme.border, LINE_MAX),
                    ("border_strong", theme.border_strong, LINE_MAX),
                ] {
                    assert!(
                        color::chroma(surface) <= limit,
                        "{palette:?}/{mode:?}: {name} has chroma {:.3}, over {limit}",
                        color::chroma(surface)
                    );
                }
                // The other half of the bargain: the accent is small, so it
                // keeps whatever colour the palette gave it. A cap applied
                // everywhere would have left the whole app grey.
                assert!(
                    color::chroma(theme.accent) > SURFACE_MAX,
                    "{palette:?}/{mode:?}: the accent went grey too"
                );
            }
        }
    }

    /// A palette can only be built in a mode it has the colours for.
    #[test]
    fn a_palette_only_offers_the_modes_it_can_actually_do() {
        // Three darks and a highlight: works either way round.
        assert_eq!(
            Theme::palette_modes([0x222831, 0x393e46, 0x00adb5, 0xeeeeee]),
            (true, true)
        );
        // All four pale — there is no dark ground in here to be had.
        assert_eq!(
            Theme::palette_modes([0xe3fdfd, 0xcbf1f5, 0xa6e3e9, 0x71c9ce]),
            (false, true)
        );
        // All four deep.
        assert_eq!(
            Theme::palette_modes([0x000000, 0x111111, 0x1a1a1a, 0x222222]),
            (true, false)
        );
    }

    /// Where a palette can do both, the setting still decides — the whole
    /// point of asking what it *can* do rather than what it *is*.
    #[test]
    fn the_setting_still_wins_on_a_palette_that_can_do_both() {
        let versatile = [0x222831, 0x393e46, 0x00adb5, 0xeeeeee];
        assert_eq!(Theme::palette_mode(versatile, Mode::Dark), Mode::Dark);
        assert_eq!(Theme::palette_mode(versatile, Mode::Light), Mode::Light);

        // And where it cannot, the palette wins whatever was asked for.
        let pastel = [0xe3fdfd, 0xcbf1f5, 0xa6e3e9, 0x71c9ce];
        assert_eq!(Theme::palette_mode(pastel, Mode::Dark), Mode::Light);
        assert_eq!(Theme::palette_mode(pastel, Mode::Light), Mode::Light);
    }

    /// The built-in themes, held to the same bar as the derived ones.
    ///
    /// `text_faint` measured 2.69:1 on the light theme — under even the 3:1
    /// floor for non-text UI — and the queue was drawing its percentage in it.
    /// That label is small text, so it needs `text_muted` and 4.5:1.
    #[test]
    fn the_built_in_themes_have_a_readable_muted_tier() {
        for chrome in [Chrome::Solid, Chrome::Glass] {
            for mode in [Mode::Dark, Mode::Light] {
                let theme = Theme::new(mode, chrome);
                let ground = theme.ground.alpha(1.0);
                for (name, surface) in [("ground", ground), ("modal", theme.modal)] {
                    assert!(
                        color::contrast(theme.text_muted, surface) >= color::CONTRAST_TEXT,
                        "{mode:?}/{chrome:?}: muted text on {name} is {:.2}:1",
                        color::contrast(theme.text_muted, surface)
                    );
                }
                // Faint is allowed to be quieter, but never below the floor
                // for anything a user has to make out at all.
                assert!(
                    color::contrast(theme.text_faint, ground) >= color::CONTRAST_UI,
                    "{mode:?}/{chrome:?}: faint text is {:.2}:1",
                    color::contrast(theme.text_faint, ground)
                );
            }
        }
    }

    /// The wordmark, on every ground the app can actually produce.
    ///
    /// Two failures sat here before it read the theme. The title-bar logo is
    /// 15px bold, which WCAG does not count as large, so it needs 4.5:1 — and
    /// the fixed `#1476f2` gave 4.03:1 on the built-in light theme and 3.56:1
    /// on Aqua Punch. Worse, the mode it was handed came from the setting
    /// rather than the theme, so a light-only palette under a dark setting
    /// painted a near-white wordmark onto near-white at 1.02:1.
    #[test]
    fn the_wordmark_is_readable_on_every_ground() {
        /// What `brand_logo` uses for the title bar, where it is smallest.
        const TITLE_BAR_SIZE: f32 = 104. * 64. / 440.;
        assert!(
            TITLE_BAR_SIZE < 18.66,
            "premise: the title bar logo is small text"
        );

        let mut themes = vec![
            Theme::new(Mode::Dark, Chrome::Solid),
            Theme::new(Mode::Light, Chrome::Solid),
        ];
        for palette in ColorPalette::ALL {
            let Some(accent) = palette.accent() else {
                continue;
            };
            let (can_dark, can_light) = Theme::palette_modes(palette.colors());
            for (mode, ok) in [(Mode::Dark, can_dark), (Mode::Light, can_light)] {
                if ok {
                    themes.push(Theme::from_palette(
                        palette.colors(),
                        Some(accent),
                        mode,
                        Chrome::Solid,
                    ));
                }
            }
        }

        for theme in themes {
            let ground = theme.ground.alpha(1.0);
            // The very colours `brand_logo` paints with, not a restatement of
            // the rule — a test that recomputed it would keep passing however
            // the logo drifted.
            let (s3, browser) = theme.wordmark(TITLE_BAR_SIZE);
            assert!(
                color::contrast(s3, ground) >= color::CONTRAST_TEXT,
                "\"S3\" is {:.2}:1 on {ground:?}",
                color::contrast(s3, ground)
            );
            assert!(
                color::contrast(browser, ground) >= color::CONTRAST_TEXT,
                "\"Browser\" is {:.2}:1 on {ground:?}",
                color::contrast(browser, ground)
            );
        }
    }

    /// The mode a theme *is*, which is not always the mode that was asked for.
    #[test]
    fn a_themes_mode_can_be_read_back_off_its_ground() {
        for palette in ColorPalette::ALL {
            let Some(accent) = palette.accent() else {
                continue;
            };
            let (can_dark, can_light) = Theme::palette_modes(palette.colors());
            for (mode, ok) in [(Mode::Dark, can_dark), (Mode::Light, can_light)] {
                if !ok {
                    continue;
                }
                let theme =
                    Theme::from_palette(palette.colors(), Some(accent), mode, Chrome::Solid);
                assert_eq!(
                    theme.mode(),
                    mode,
                    "{palette:?} built in {mode:?} but its ground reads otherwise"
                );
            }
        }
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
