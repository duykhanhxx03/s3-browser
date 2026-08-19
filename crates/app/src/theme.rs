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
}

impl Theme {
    pub fn new(mode: Mode, chrome: Chrome) -> Self {
        match mode {
            Mode::Dark => Self {
                ground: if chrome.is_glass() {
                    rgba(0x12151bdb).into()
                } else {
                    rgb(0x12151b).into()
                },
                panel: rgba(0xffffff0d).into(),
                modal: rgb(0x1c2029).into(),
                hover: rgba(0xffffff14).into(),
                selected: rgba(0x5ca8ff3d).into(),
                drop_target: rgba(0x5ca8ff29).into(),

                text: rgb(0xe6eaf0).into(),
                text_muted: rgba(0xffffff9e).into(),
                text_faint: rgba(0xffffff66).into(),
                text_on_accent: rgb(0xffffff).into(),

                border: rgba(0xffffff17).into(),
                border_strong: rgba(0xffffff2b).into(),

                accent: rgb(0x5ca8ff).into(),
                danger: rgb(0xe5695b).into(),
            },
            Mode::Light => Self {
                ground: if chrome.is_glass() {
                    rgba(0xf2f4f8c7).into()
                } else {
                    rgb(0xf2f4f8).into()
                },
                // On a light ground the panels read as *lighter*, not darker.
                panel: rgba(0xffffff8c).into(),
                modal: rgb(0xfbfcfe).into(),
                hover: rgba(0x1b243014).into(),
                selected: rgba(0x0a6cdb26).into(),
                drop_target: rgba(0x0a6cdb1f).into(),

                text: rgb(0x1b2430).into(),
                text_muted: rgba(0x1b2430a3).into(),
                text_faint: rgba(0x1b243070).into(),
                text_on_accent: rgb(0xffffff).into(),

                border: rgba(0x1b24301a).into(),
                border_strong: rgba(0x1b24302e).into(),

                accent: rgb(0x0a6cdb).into(),
                danger: rgb(0xc03a2b).into(),
            },
        }
    }

    pub fn from_window(appearance: WindowAppearance, chrome: Chrome) -> Self {
        Self::new(Mode::from_appearance(appearance), chrome)
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
            for appearance in [WindowAppearance::Dark, WindowAppearance::Light] {
                let theme = Theme::from_window(appearance, chrome);
                // A dialog floats over content. Any transparency here and the
                // list underneath reads through the text on top of it.
                assert_eq!(
                    theme.modal.a, 1.0,
                    "modal must be opaque for {chrome:?}/{appearance:?}"
                );
                // The panel is deliberately not opaque; if these ever became
                // the same value the distinction would have been lost.
                assert!(theme.panel.a < 1.0);
            }
        }
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
    fn text_contrasts_with_its_ground_in_both_modes() {
        // Rough luminance check: dark mode paints light text, light mode dark text.
        let dark = Theme::new(Mode::Dark, Chrome::Solid);
        let light = Theme::new(Mode::Light, Chrome::Solid);
        assert!(dark.text.l > dark.ground.l, "dark mode needs light text");
        assert!(light.text.l < light.ground.l, "light mode needs dark text");
    }
}
