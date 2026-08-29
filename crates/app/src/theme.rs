//! Colour tokens for light and dark, in glass and solid chrome.
//!
//! Only the window ground changes between chromes: in glass it is translucent so
//! the compositor's blur shows through, in solid it is opaque. Every panel above
//! it is an alpha overlay, so it composites correctly either way and there is no
//! second palette to keep in sync.

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

        let ground = if dark {
            ordered[0]
        } else {
            ordered[ordered.len() - 1]
        };
        let ground_l = color::lightness(ground);

        // Text is the palette's own far end when that end is readable, and
        // the same hue pushed until it is when it is not. Aimed past AA at
        // 7:1, because body text is what the whole window is made of.
        let far = if dark {
            ordered[ordered.len() - 1]
        } else {
            ordered[0]
        };
        let text = color::lift_until(far, ground, 7.0, if dark { 1.0 } else { 0.0 });

        // Surfaces step away from the ground in perceptual lightness, so the
        // ramp looks even whatever hue the palette happens to be — and each
        // step is pulled back if it drifts far enough to cost the body text
        // its 4.5:1.
        let step = if dark { 0.055 } else { -0.045 };
        let near = if dark {
            ordered[1]
        } else {
            ordered[ordered.len() - 2]
        };
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

        Self {
            // Glass keeps the palette's ground but lets the compositor
            // through, exactly as the built-in theme does.
            ground: if chrome.is_glass() {
                ground.alpha(if dark { 0.86 } else { 0.96 })
            } else {
                ground
            },
            panel: surface(near, 1.0),
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

            border: color::with_lightness(ground, ground_l + step * 3.0),
            border_strong: color::with_lightness(ground, ground_l + step * 4.4),

            accent,
            danger: color::lift_until(
                rgb(if dark { 0xff6b6b } else { 0xc03a2b }).into(),
                ground,
                color::CONTRAST_UI,
                limit,
            ),
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

    /// The point of the rewrite: a palette's own colours reach the window,
    /// rather than being washed over the built-in ones.
    #[test]
    fn a_palette_actually_paints_its_own_ground() {
        for palette in ColorPalette::ALL {
            let Some(accent) = palette.accent() else {
                continue;
            };
            let mode = Theme::palette_mode(palette.colors(), Mode::Dark);
            let theme = Theme::from_palette(palette.colors(), Some(accent), mode, Chrome::Solid);
            let is_one_of_the_palettes_own = palette
                .colors()
                .iter()
                .any(|hex| Hsla::from(rgb(*hex)) == theme.ground);
            assert!(
                is_one_of_the_palettes_own,
                "{palette:?} ground {:?} is not a colour from the palette",
                theme.ground
            );
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

    #[test]
    fn text_contrasts_with_its_ground_in_both_modes() {
        // Rough luminance check: dark mode paints light text, light mode dark text.
        let dark = Theme::new(Mode::Dark, Chrome::Solid);
        let light = Theme::new(Mode::Light, Chrome::Solid);
        assert!(dark.text.l > dark.ground.l, "dark mode needs light text");
        assert!(light.text.l < light.ground.l, "light mode needs dark text");
    }
}
