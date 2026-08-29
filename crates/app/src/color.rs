//! Colour maths for deriving a readable theme from an arbitrary palette.
//!
//! Two things live here that gpui's `Hsla` cannot do, and both matter:
//!
//! **Relative luminance and contrast**, so readability is measured rather than
//! guessed. HSL lightness is not perceptual — `#ffd700` and `#0000ff` are both
//! `l = 0.50`, yet the yellow reflects ten times the light. Every "is this
//! light or dark" question answered from `Hsla::l` gets colours like those
//! backwards, which is how a teal accent ended up carrying white text at
//! 2.75:1.
//!
//! **OKLab**, so lightness can be *changed* without the hue sliding. Equal
//! steps in OKLab's L look equal at any hue, which is what makes a surface
//! ramp built on one palette look like the same ramp built on another.
//!
//! Both are standard: luminance and contrast from WCAG 2, OKLab from
//! Björn Ottosson's 2020 derivation.

use gpui::{Hsla, Rgba};

/// WCAG AA for body text.
pub const CONTRAST_TEXT: f32 = 4.5;
/// WCAG AA for large text, icons and other non-text UI.
pub const CONTRAST_UI: f32 = 3.0;

fn to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(channel: f32) -> f32 {
    let channel = channel.clamp(0., 1.);
    if channel <= 0.0031308 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1. / 2.4) - 0.055
    }
}

/// WCAG relative luminance: how much light the colour actually returns.
pub fn luminance(color: Hsla) -> f32 {
    let rgba = Rgba::from(color);
    0.2126 * to_linear(rgba.r) + 0.7152 * to_linear(rgba.g) + 0.0722 * to_linear(rgba.b)
}

/// WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white).
pub fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Perceptual lightness, 0.0 to 1.0.
pub fn lightness(color: Hsla) -> f32 {
    oklab(color).0
}

/// How colourful, regardless of how light. Zero for any grey.
pub fn chroma(color: Hsla) -> f32 {
    let (_, a, b) = oklab(color);
    a.hypot(b)
}

// Written out at Ottosson's published precision rather than trimmed to what
// f32 can hold. They are quoted constants: anyone checking this against the
// reference should find the same digits, and the compiler rounds them for us.
#[allow(clippy::excessive_precision)]
fn oklab(color: Hsla) -> (f32, f32, f32) {
    let rgba = Rgba::from(color);
    let (r, g, b) = (to_linear(rgba.r), to_linear(rgba.g), to_linear(rgba.b));

    let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;
    let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());

    (
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    )
}

/// The same colour at a different perceptual lightness — same hue, same
/// colourfulness, only brighter or darker.
// Constants at Ottosson's published precision, as above.
#[allow(clippy::excessive_precision)]
pub fn with_lightness(color: Hsla, lightness: f32) -> Hsla {
    let (_, a, b) = oklab(color);
    let alpha = color.a;

    let l_ = lightness + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = lightness - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = lightness - 0.0894841775 * a - 1.2914855480 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    let mut out = Hsla::from(Rgba {
        r: from_linear(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        g: from_linear(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        b: from_linear(-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s),
        a: 1.0,
    });
    out.a = alpha;
    out
}

/// Walks `color` toward `against` and stops at the last point that still
/// clears `target`.
///
/// Used for the muted and faint text tiers. Deriving those by subtracting a
/// fixed amount of lightness is what made them fail: a step that looks right
/// against a near-black ground walks straight past the legibility minimum
/// against a mid-grey one, and the palette decides which ground you get.
pub fn fade_toward(color: Hsla, against: Hsla, target: f32) -> Hsla {
    let from = lightness(color);
    let to = lightness(against);
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    let mut best = color;
    for _ in 0..24 {
        let mid = (lo + hi) / 2.;
        let candidate = with_lightness(color, from + (to - from) * mid);
        if contrast(candidate, against) >= target {
            best = candidate;
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best
}

/// Pushes `color` toward `limit` lightness until it clears `target` against
/// `against`, keeping its hue for as long as it can.
pub fn lift_until(color: Hsla, against: Hsla, target: f32, limit: f32) -> Hsla {
    if contrast(color, against) >= target {
        return color;
    }
    let start = lightness(color);
    for step in 1..=40 {
        let candidate = with_lightness(color, start + (limit - start) * (step as f32 / 40.));
        if contrast(candidate, against) >= target {
            return candidate;
        }
    }
    with_lightness(color, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;

    #[test]
    fn oklab_round_trips_every_srgb_corner() {
        for hex in [
            0x000000, 0xffffff, 0xff0000, 0x00ff00, 0x0000ff, 0xffd700, 0x00adb5, 0x222831,
            0xe3fdfd, 0x3f72af, 0xff2e63, 0x71c9ce, 0x7f7f7f, 0x123456,
        ] {
            let color: Hsla = rgb(hex).into();
            let round_tripped = with_lightness(color, lightness(color));
            let (a, b) = (Rgba::from(color), Rgba::from(round_tripped));
            for (from, to) in [(a.r, b.r), (a.g, b.g), (a.b, b.b)] {
                assert!(
                    (from - to).abs() < 0.004,
                    "#{hex:06x} did not survive the OKLab round trip: {from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn luminance_separates_what_hsl_lightness_confuses() {
        // The bug this module exists to end: identical HSL lightness, an
        // order of magnitude apart in the light they actually return.
        let yellow: Hsla = rgb(0xffd700).into();
        let blue: Hsla = rgb(0x0000ff).into();
        assert!(
            (yellow.l - blue.l).abs() < 0.01,
            "premise: same HSL lightness"
        );
        assert!(
            luminance(yellow) > luminance(blue) * 5.,
            "yellow {} vs blue {}",
            luminance(yellow),
            luminance(blue)
        );
        assert!(lightness(yellow) > lightness(blue));
    }

    #[test]
    fn contrast_matches_the_wcag_reference_points() {
        let black: Hsla = rgb(0x000000).into();
        let white: Hsla = rgb(0xffffff).into();
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 0.001);
        // Symmetric, whichever way round it is asked.
        let teal: Hsla = rgb(0x00adb5).into();
        assert!((contrast(teal, white) - contrast(white, teal)).abs() < 0.001);
    }

    #[test]
    fn fading_never_crosses_the_target_it_was_given() {
        for ground in [0x0e0f12, 0xf7f8f9, 0x222831, 0xe3fdfd, 0x808080] {
            let ground: Hsla = rgb(ground).into();
            for text in [0xffffff, 0x000000, 0x3f72af, 0xff2e63] {
                let text: Hsla = rgb(text).into();
                if contrast(text, ground) < CONTRAST_TEXT {
                    continue;
                }
                let faded = fade_toward(text, ground, CONTRAST_TEXT);
                assert!(
                    contrast(faded, ground) >= CONTRAST_TEXT - 0.01,
                    "faded past the minimum it was asked to keep"
                );
            }
        }
    }
}
