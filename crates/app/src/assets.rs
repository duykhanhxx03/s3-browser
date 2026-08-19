//! Icons, compiled into the binary.
//!
//! They are embedded rather than read from disk because a `.app` bundle that
//! loses its resources renders every button blank, and that failure would only
//! show up after packaging — long after the code looked correct.
//!
//! The set is hand-authored on one grid: 24×24, 1.75 stroke, round caps and
//! joins, `currentColor` so the theme tints them. Mixing icons drawn to
//! different weights is what makes an interface look assembled from parts.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

/// One entry per file in `assets/icons`.
macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"),
               include_bytes!(concat!("../../../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons![
    "arrow-up", "check", "close", "download", "external", "eye", "file", "folder", "info", "link",
    "pause", "play", "plus", "refresh", "trash", "upload",
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads_and_is_a_svg() {
        for (path, _) in ICONS {
            let bytes = Assets
                .load(path)
                .unwrap()
                .unwrap_or_else(|| panic!("{path} did not load"));
            let text = std::str::from_utf8(&bytes).unwrap();

            assert!(text.starts_with("<svg"), "{path} is not an SVG");
            // Without `currentColor` an icon ignores the theme and stays one
            // colour in both light and dark mode.
            assert!(
                text.contains("currentColor"),
                "{path} does not follow the text colour"
            );
            // One grid for all of them; a stray viewBox makes an icon render at
            // a different visual weight beside its neighbours.
            assert!(text.contains(r#"viewBox="0 0 24 24""#), "{path} is off-grid");
            assert!(text.contains(r#"stroke-width="1.75""#), "{path} is off-weight");
        }
    }

    #[test]
    fn a_missing_icon_is_none_rather_than_an_error() {
        // gpui asks for assets it may not have; erroring would take down the
        // frame instead of just leaving a gap.
        assert!(Assets.load("icons/does-not-exist.svg").unwrap().is_none());
    }

    #[test]
    fn listing_is_scoped_to_the_prefix() {
        assert_eq!(Assets.list("icons/").unwrap().len(), ICONS.len());
        assert!(Assets.list("nothing/").unwrap().is_empty());
    }
}
