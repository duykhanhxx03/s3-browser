//! Fetching palettes from Color Hunt, across its sort orders and tag
//! categories.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::settings::CustomColorPalette;

const FEED_URL: &str = "https://colorhunt.co/php/feed.php";
const TIMEFRAME_ALL: &str = "4000";
/// One page returns 40 palettes; two pages give a decent spread per tab
/// without hammering the feed on every click.
const PAGES_PER_CATEGORY: usize = 2;

#[derive(Deserialize)]
struct FeedItem {
    code: String,
}

/// A Color Hunt sort order or tag. The list mirrors the categories Color
/// Hunt's own site links from its navigation (colorhunt.co/palettes/<tag>);
/// each one was confirmed live against the feed before being added here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Category {
    /// Stable id, also the locale suffix: `color_hunt.category.{key}`.
    pub key: &'static str,
    sort: &'static str,
    tag: &'static str,
}

pub const CATEGORIES: &[Category] = &[
    Category {
        key: "new",
        sort: "new",
        tag: "",
    },
    Category {
        key: "popular",
        sort: "popular",
        tag: "",
    },
    Category {
        key: "random",
        sort: "random",
        tag: "",
    },
    tag_category("pastel"),
    tag_category("vintage"),
    tag_category("retro"),
    tag_category("neon"),
    tag_category("gold"),
    tag_category("warm"),
    tag_category("dark"),
    tag_category("light"),
    tag_category("gradient"),
    tag_category("rainbow"),
    tag_category("happy"),
    tag_category("nature"),
    tag_category("earth"),
    tag_category("sky"),
    tag_category("sea"),
    tag_category("space"),
    tag_category("night"),
    tag_category("cold"),
    tag_category("spring"),
    tag_category("summer"),
    tag_category("fall"),
    tag_category("winter"),
    tag_category("sunset"),
    tag_category("food"),
    tag_category("coffee"),
    tag_category("cream"),
    tag_category("skin"),
    tag_category("kids"),
    tag_category("wedding"),
    tag_category("christmas"),
    tag_category("halloween"),
];

const fn tag_category(tag: &'static str) -> Category {
    Category {
        key: tag,
        sort: "popular",
        tag,
    }
}

pub async fn fetch_category(category: Category) -> Result<Vec<CustomColorPalette>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("s3browser/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building Color Hunt client")?;
    let mut palettes = Vec::new();

    for step in 0..PAGES_PER_CATEGORY {
        let step = step.to_string();
        let response = client
            .post(FEED_URL)
            .form(&[
                ("step", step.as_str()),
                ("sort", category.sort),
                ("tags", category.tag),
                ("timeframe", TIMEFRAME_ALL),
            ])
            .send()
            .await
            .context("fetching Color Hunt palettes")?
            .error_for_status()
            .context("Color Hunt returned an error")?;

        let body = response
            .bytes()
            .await
            .context("reading Color Hunt palette feed")?;
        let page: Vec<FeedItem> =
            serde_json::from_slice(&body).context("parsing Color Hunt palette feed")?;
        if page.is_empty() {
            break;
        }

        for item in page {
            let colors = colors_from_code(&item.code)?;
            palettes.push(CustomColorPalette::new(
                format!("Color Hunt {}", item.code.to_ascii_uppercase()),
                colors,
            ));
        }
    }

    palettes.dedup_by(|a, b| a.colors == b.colors);
    Ok(palettes)
}

fn colors_from_code(code: &str) -> Result<[u32; 4]> {
    let code = code.trim();
    if code.len() != 24 || !code.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid Color Hunt palette code: {code}");
    }

    let mut colors = [0; 4];
    for (ix, color) in colors.iter_mut().enumerate() {
        let start = ix * 6;
        *color = u32::from_str_radix(&code[start..start + 6], 16)
            .with_context(|| format!("parsing Color Hunt palette code: {code}"))?;
    }
    Ok(colors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_hunt_codes_are_four_hex_colours() {
        assert_eq!(
            colors_from_code("222831393e4600adb5eeeeee").unwrap(),
            [0x222831, 0x393e46, 0x00adb5, 0xeeeeee]
        );
        assert!(colors_from_code("222831").is_err());
        assert!(colors_from_code("222831393e4600adb5eeeeeg").is_err());
    }

    #[test]
    fn every_category_key_is_unique() {
        let mut keys: Vec<&str> = CATEGORIES.iter().map(|c| c.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), CATEGORIES.len());
    }
}
