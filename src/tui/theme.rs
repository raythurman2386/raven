//! Color themes for the TUI.
//!
//! A [`Theme`] is a plain data struct of `Color` values (plus the two RGB
//! triples used for the tool-call "glimmer" interpolation). Themes are
//! `Copy`, so they can be threaded through the render functions cheaply and
//! swapped at runtime — e.g. via the `/theme` slash command.
//!
//! Built-in presets live in [`Theme::all`] and are looked up by name with
//! [`Theme::by_name`]. Adding a new theme is one `const` plus one registry
//! entry.

use ratatui::style::Color;

/// A complete color palette for the TUI.
///
/// `tool_rgb` / `dim_rgb` are the RGB components of the tool (orange) and dim
/// (grey) accents, used to interpolate the tool-call glimmer as it fades.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Primary foreground (body text).
    pub fg: Color,
    /// Muted / secondary foreground (metadata, gutters, faded tools).
    pub dim: Color,
    /// Hero accent (Raven tag, headings, links, usage readout).
    pub accent: Color,
    /// User / "ready" accent.
    pub user: Color,
    /// Tool-call accent (orange).
    pub tool: Color,
    /// System-message foreground.
    pub system: Color,
    /// Error foreground.
    pub error: Color,
    /// Plan-panel accent (purple).
    pub plan: Color,
    /// Log border color.
    pub border: Color,
    /// Status-strip background.
    pub status_bg: Color,
    /// Visual-selection background.
    pub select_bg: Color,
    /// RGB of the tool accent, for glimmer interpolation.
    pub tool_rgb: (u8, u8, u8),
    /// RGB of the dim accent, for glimmer interpolation.
    pub dim_rgb: (u8, u8, u8),
}

impl Theme {
    /// The default Ravenwood emerald-forest palette (warm beige foreground,
    /// olive-tinged backgrounds, green hero accent, pastel brights).
    pub const RAVENWOOD: Theme = Theme {
        fg: Color::Rgb(0xE8, 0xD5, 0xB7),        // warm beige
        dim: Color::Rgb(0x85, 0x92, 0x89),       // grey1
        accent: Color::Rgb(0x22, 0xD3, 0xEE),    // blue
        user: Color::Rgb(0x4A, 0xDE, 0x80),      // green — hero
        tool: Color::Rgb(0xE6, 0x98, 0x75),      // orange
        system: Color::Rgb(0x7F, 0x89, 0x7D),    // grey0
        error: Color::Rgb(0xE6, 0x7E, 0x80),     // red
        plan: Color::Rgb(0xF4, 0x72, 0xB6),      // purple
        border: Color::Rgb(0x4A, 0x5A, 0x4D),    // bg4
        status_bg: Color::Rgb(0x1F, 0x24, 0x1F), // bg1
        select_bg: Color::Rgb(0x3A, 0x4F, 0x3D), // bg visual selection
        tool_rgb: (0xE6, 0x98, 0x75),
        dim_rgb: (0x85, 0x92, 0x89),
    };

    /// Nord — the arctic, north-bluish palette.
    pub const NORD: Theme = Theme {
        fg: Color::Rgb(0xD8, 0xDE, 0xE9),        // nord4
        dim: Color::Rgb(0x4C, 0x56, 0x6A),       // nord3
        accent: Color::Rgb(0x88, 0xC0, 0xD0),    // nord8
        user: Color::Rgb(0xA3, 0xBE, 0x8C),      // nord14
        tool: Color::Rgb(0xD0, 0x87, 0x70),      // nord12
        system: Color::Rgb(0x61, 0x6E, 0x88),    // nord3-ish
        error: Color::Rgb(0xBF, 0x61, 0x6A),     // nord11
        plan: Color::Rgb(0xB4, 0x8E, 0xAD),      // nord15
        border: Color::Rgb(0x3B, 0x42, 0x52),    // nord1
        status_bg: Color::Rgb(0x2E, 0x34, 0x40), // nord0
        select_bg: Color::Rgb(0x43, 0x4C, 0x5E), // nord2
        tool_rgb: (0xD0, 0x87, 0x70),
        dim_rgb: (0x4C, 0x56, 0x6A),
    };

    /// Dracula — the classic dark purple/cyan palette.
    pub const DRACULA: Theme = Theme {
        fg: Color::Rgb(0xF8, 0xF8, 0xF2),
        dim: Color::Rgb(0x62, 0x72, 0xA4),
        accent: Color::Rgb(0x8B, 0xE9, 0xFD),
        user: Color::Rgb(0x50, 0xFA, 0x7B),
        tool: Color::Rgb(0xFF, 0xB8, 0x6C),
        system: Color::Rgb(0x62, 0x72, 0xA4),
        error: Color::Rgb(0xFF, 0x55, 0x55),
        plan: Color::Rgb(0xFF, 0x79, 0xC6),
        border: Color::Rgb(0x44, 0x47, 0x5A),
        status_bg: Color::Rgb(0x28, 0x2A, 0x36),
        select_bg: Color::Rgb(0x44, 0x47, 0x5A),
        tool_rgb: (0xFF, 0xB8, 0x6C),
        dim_rgb: (0x62, 0x72, 0xA4),
    };

    /// Solarized Dark — the low-contrast, warm/cool balanced palette.
    pub const SOLARIZED_DARK: Theme = Theme {
        fg: Color::Rgb(0x93, 0xA1, 0xA1),        // base0
        dim: Color::Rgb(0x58, 0x6E, 0x75),       // base01
        accent: Color::Rgb(0x26, 0x8B, 0xD2),    // blue
        user: Color::Rgb(0x85, 0x99, 0x00),      // green
        tool: Color::Rgb(0xCB, 0x4B, 0x16),      // orange
        system: Color::Rgb(0x65, 0x7B, 0x83),    // base00
        error: Color::Rgb(0xDC, 0x32, 0x2F),     // red
        plan: Color::Rgb(0xD3, 0x36, 0x82),      // magenta
        border: Color::Rgb(0x07, 0x36, 0x42),    // base02
        status_bg: Color::Rgb(0x00, 0x2B, 0x36), // base03
        select_bg: Color::Rgb(0x07, 0x36, 0x42), // base02
        tool_rgb: (0xCB, 0x4B, 0x16),
        dim_rgb: (0x58, 0x6E, 0x75),
    };

    /// The registry of built-in themes: `(name, theme)`.
    pub fn all() -> &'static [(&'static str, Theme)] {
        &[
            ("ravenwood", Theme::RAVENWOOD),
            ("nord", Theme::NORD),
            ("dracula", Theme::DRACULA),
            ("solarized-dark", Theme::SOLARIZED_DARK),
        ]
    }

    /// Look up a theme by name (case-insensitive). Returns `None` if unknown.
    pub fn by_name(name: &str) -> Option<Theme> {
        let name = name.trim().to_ascii_lowercase();
        Theme::all()
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
    }

    /// The default theme (Ravenwood).
    pub const fn default_theme() -> Theme {
        Theme::RAVENWOOD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_finds_presets_case_insensitively() {
        assert_eq!(Theme::by_name("nord"), Some(Theme::NORD));
        assert_eq!(Theme::by_name("NORD"), Some(Theme::NORD));
        assert_eq!(Theme::by_name("Dracula"), Some(Theme::DRACULA));
        assert_eq!(
            Theme::by_name("solarized-dark"),
            Some(Theme::SOLARIZED_DARK)
        );
    }

    #[test]
    fn by_name_unknown_returns_none() {
        assert_eq!(Theme::by_name("nope"), None);
        assert_eq!(Theme::by_name(""), None);
    }

    #[test]
    fn all_has_unique_names_and_includes_default() {
        let names: Vec<&str> = Theme::all().iter().map(|(n, _)| *n).collect();
        for (i, a) in names.iter().enumerate() {
            for b in names.iter().skip(i + 1) {
                assert_ne!(a, b, "duplicate theme name {a}");
            }
        }
        assert!(
            names.contains(&"ravenwood"),
            "default theme must be registered"
        );
    }

    #[test]
    fn default_is_ravenwood() {
        assert_eq!(Theme::default_theme(), Theme::RAVENWOOD);
    }
}
