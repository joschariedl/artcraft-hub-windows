//! Omarchy theme colours.
//!
//! Omarchy writes the active theme to
//! `~/.local/state/omarchy/current/theme/colors.toml`. The hub reads that file
//! at startup so it blends in with the rest of the desktop. When the file is
//! missing or incomplete, the original ArtCraft palette is used as a fallback.

use std::fs;

use crate::paths::Paths;

pub type Rgb = [u8; 3];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: Option<String>,
    pub dark: bool,
    pub background: Rgb,
    pub panel: Rgb,
    pub card: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub bad: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: None,
            dark: true,
            background: [13, 17, 24],
            panel: [18, 23, 32],
            card: [24, 30, 41],
            border: [42, 51, 68],
            text: [232, 236, 244],
            muted: [144, 156, 178],
            accent: [64, 208, 180],
            ok: [92, 200, 130],
            warn: [240, 180, 80],
            bad: [240, 105, 115],
        }
    }
}

impl Theme {
    /// Loads the active Omarchy theme, falling back to the built-in palette.
    pub fn load(paths: &Paths) -> Self {
        let name = fs::read_to_string(paths.omarchy_theme_name_file())
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let Ok(text) = fs::read_to_string(paths.omarchy_theme_file()) else {
            return Self {
                name,
                ..Self::default()
            };
        };
        Self::from_toml(&text, name)
    }

    /// Parses the small `key = "#rrggbb"` file Omarchy writes. Unknown keys and
    /// malformed values are ignored so a partial theme still works.
    pub fn from_toml(text: &str, name: Option<String>) -> Self {
        let fallback = Self {
            name,
            ..Self::default()
        };

        let mut colors = std::collections::BTreeMap::new();
        let mut dark = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches(['"', '\'']);
            if key == "mode" {
                dark = Some(!value.eq_ignore_ascii_case("light"));
                continue;
            }
            if let Some(rgb) = parse_hex(value) {
                colors.insert(key, rgb);
            }
        }

        let pick = |key: &str, fallback: Rgb| colors.get(key).copied().unwrap_or(fallback);
        let background = pick("background", fallback.background);
        let text = pick("foreground", fallback.text);
        let panel = pick(
            "dark_background",
            mix(background, [0, 0, 0], if dark.unwrap_or(true) { 0.28 } else { 0.04 }),
        );
        let card = pick("lighter_background", mix(background, text, 0.06));
        let border = pick("selection", mix(card, text, 0.18));
        let muted = pick("muted", mix(text, background, 0.42));

        Self {
            name: fallback.name,
            dark: dark.unwrap_or(true),
            background,
            panel,
            card,
            border,
            text,
            muted,
            accent: pick("accent", fallback.accent),
            ok: pick("green", fallback.ok),
            warn: pick("yellow", fallback.warn),
            bad: pick("red", fallback.bad),
        }
    }
}

/// Parses `#rrggbb` or `#rrggbbaa` (alpha is discarded).
pub fn parse_hex(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    let hex = if hex.len() == 8 { &hex[..6] } else { hex };
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16).ok()
    };
    Some([channel(0..2)?, channel(2..4)?, channel(4..6)?])
}

/// Linear mix between two colours (`amount = 0` keeps `a`).
pub fn mix(a: Rgb, b: Rgb, amount: f32) -> Rgb {
    let amount = amount.clamp(0.0, 1.0);
    let blend = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * amount).round().clamp(0.0, 255.0) as u8
    };
    [blend(a[0], b[0]), blend(a[1], b[1]), blend(a[2], b[2])]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_values() {
        assert_eq!(parse_hex("#509475"), Some([0x50, 0x94, 0x75]));
        assert_eq!(parse_hex("#509475ff"), Some([0x50, 0x94, 0x75]));
        assert_eq!(parse_hex("509475"), None);
        assert_eq!(parse_hex("#xyzxyz"), None);
    }

    #[test]
    fn reads_an_omarchy_theme() {
        let text = r##"
mode = "dark"

accent = "#509475"
selection = "#32473B"
muted = "#53685B"

background = "#111c18"
dark_background = "#0c1512"
darker_background = "#090f0d"
lighter_background = "#23372B"

foreground = "#C1C497"
red = "#FF5345"
yellow = "#459451"
green = "#549e6a"
"##;
        let theme = Theme::from_toml(text, Some("osaka-jade".into()));
        assert!(theme.dark);
        assert_eq!(theme.name.as_deref(), Some("osaka-jade"));
        assert_eq!(theme.background, [0x11, 0x1c, 0x18]);
        assert_eq!(theme.panel, [0x0c, 0x15, 0x12]);
        assert_eq!(theme.card, [0x23, 0x37, 0x2b]);
        assert_eq!(theme.accent, [0x50, 0x94, 0x75]);
        assert_eq!(theme.ok, [0x54, 0x9e, 0x6a]);
    }

    #[test]
    fn incomplete_themes_keep_the_defaults() {
        let theme = Theme::from_toml("mode = \"light\"\naccent = \"#ffffff\"\n", None);
        assert!(!theme.dark);
        assert_eq!(theme.accent, [255, 255, 255]);
        assert_eq!(theme.bad, Theme::default().bad);
    }
}
