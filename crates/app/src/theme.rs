//! Colour tokens for light and dark, in glass and solid chrome.
//!
//! Only the window ground changes between chromes: in glass it is translucent so
//! the compositor's blur shows through, in solid it is opaque. Every panel above
//! it is an alpha overlay, so it composites correctly either way and there is no
//! second palette to keep in sync.

use gpui::{rgb, rgba, Hsla, WindowAppearance};

use crate::platform::Chrome;
use crate::settings::ColorPalette;

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

    pub fn with_color_palette(mut self, palette: ColorPalette, mode: Mode) -> Self {
        let Some(accent) = palette.accent() else {
            return self;
        };
        self.apply_palette(palette.colors(), Some(accent), mode);
        self
    }

    pub fn with_custom_color_palette(mut self, colors: [u32; 4], mode: Mode) -> Self {
        self.apply_palette(colors, None, mode);
        self
    }

    fn apply_palette(&mut self, colors: [u32; 4], accent: Option<u32>, mode: Mode) {
        let roles = PaletteRoles::new(colors, mode);

        self.ground = self.ground.blend(roles.ground.alpha(match mode {
            Mode::Dark => 0.1,
            Mode::Light => 0.18,
        }));
        self.panel = self.panel.blend(roles.panel.alpha(match mode {
            Mode::Dark => 0.5,
            Mode::Light => 0.22,
        }));
        self.modal = self.modal.blend(roles.modal.alpha(match mode {
            Mode::Dark => 0.16,
            Mode::Light => 0.14,
        }));
        self.hover = self.hover.blend(roles.hover.alpha(match mode {
            Mode::Dark => 0.48,
            Mode::Light => 0.28,
        }));
        self.border = self.border.blend(roles.border.alpha(match mode {
            Mode::Dark => 0.42,
            Mode::Light => 0.3,
        }));
        self.border_strong = self.border_strong.blend(roles.border.alpha(match mode {
            Mode::Dark => 0.5,
            Mode::Light => 0.36,
        }));

        let accent = readable_accent(
            rgb(accent.unwrap_or_else(|| accent_from_colors(colors))).into(),
            mode,
        );
        self.accent = accent;
        self.selected = accent.alpha(match mode {
            Mode::Dark => 0.2,
            Mode::Light => 0.17,
        });
        self.drop_target = accent.alpha(match mode {
            Mode::Dark => 0.16,
            Mode::Light => 0.14,
        });
        self.text_on_accent = if accent.l > 0.62 {
            rgb(0x1c2024).into()
        } else {
            rgb(0xffffff).into()
        };
    }
}

fn readable_accent(mut color: Hsla, mode: Mode) -> Hsla {
    if color.s < 0.28 {
        color.s = 0.28;
    }
    match mode {
        Mode::Light => color.l = color.l.clamp(0.32, 0.48),
        Mode::Dark => color.l = color.l.clamp(0.52, 0.68),
    }
    color.a = 1.0;
    color
}

#[derive(Clone, Copy)]
struct PaletteRoles {
    ground: Hsla,
    panel: Hsla,
    modal: Hsla,
    hover: Hsla,
    border: Hsla,
}

impl PaletteRoles {
    fn new(colors: [u32; 4], mode: Mode) -> Self {
        let mut colors = colors.map(|hex| Hsla::from(rgb(hex)));
        colors.sort_by(|a, b| a.l.total_cmp(&b.l));

        match mode {
            Mode::Dark => Self {
                ground: colors[0],
                panel: colors[1],
                modal: colors[1],
                hover: colors[2],
                border: colors[3],
            },
            Mode::Light => Self {
                ground: colors[3],
                panel: colors[2],
                modal: colors[3],
                hover: colors[1],
                border: colors[0],
            },
        }
    }
}

fn accent_from_colors(colors: [u32; 4]) -> u32 {
    colors
        .into_iter()
        .max_by(|a, b| {
            let a = Hsla::from(rgb(*a));
            let b = Hsla::from(rgb(*b));
            accent_score(a).total_cmp(&accent_score(b))
        })
        .unwrap_or(0x3b82f6)
}

fn accent_score(color: Hsla) -> f32 {
    let balanced_lightness = 1.0 - (color.l - 0.5).abs() * 2.0;
    color.s * 0.7 + balanced_lightness.max(0.0) * 0.3
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
    fn color_palettes_keep_the_accent_readable() {
        for palette in ColorPalette::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let theme = Theme::new(mode, Chrome::Solid).with_color_palette(palette, mode);
                assert!(
                    (theme.accent.l - theme.ground.l).abs() > 0.2,
                    "{palette:?}/{mode:?} accent does not stand off its ground"
                );
                assert!((theme.selected.h - theme.accent.h).abs() < 0.02);
                assert_eq!(theme.accent.a, 1.0);
            }
        }
    }

    #[test]
    fn color_palettes_affect_the_surfaces_too() {
        for palette in ColorPalette::ALL {
            if palette == ColorPalette::Default {
                continue;
            }
            for mode in [Mode::Dark, Mode::Light] {
                let base = Theme::new(mode, Chrome::Solid);
                let themed = base.with_color_palette(palette, mode);
                assert_ne!(themed.ground, base.ground, "{palette:?}/{mode:?} ground");
                assert_ne!(themed.panel, base.panel, "{palette:?}/{mode:?} panel");
                assert_ne!(themed.modal, base.modal, "{palette:?}/{mode:?} modal");
                assert_ne!(themed.hover, base.hover, "{palette:?}/{mode:?} hover");
                assert_ne!(themed.border, base.border, "{palette:?}/{mode:?} border");
                assert_ne!(themed.accent, base.accent, "{palette:?}/{mode:?} accent");
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
