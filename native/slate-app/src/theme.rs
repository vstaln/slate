//! Slate's design tokens.
//!
//! These are not "a dark theme" — they are the exact values vendored from the
//! old Electron renderer's `tokens.ts` (see `tests/fixtures/tokens.ts`), and a
//! test reads that fixture to keep them so.
//! The palette is deliberately near-black and almost hueless: the canvas is a
//! backdrop for terminals full of coloured output, and any tint in the chrome
//! fights with it.
//!
//! Ported as data rather than as egui styling calls so the same numbers can
//! drive the canvas, the widget chrome and anything added later, instead of
//! being retyped per call site.

/// RGB triples — same hex as before, no egui dependency.
/// Feed directly to gpui::rgb(), e.g. rgb(0x080808).
pub type Rgb = (u8, u8, u8);

/// The five greys everything is built from.
pub mod monochrome {
    use super::Rgb;
    /// The canvas itself, and the body of a terminal.
    pub const BASE: Rgb = (0x08, 0x08, 0x08);
    pub const TERMINAL_HEADER: Rgb = (0x0D, 0x0D, 0x0D);
    /// A widget's body, and the title bar.
    pub const SURFACE: Rgb = (0x12, 0x12, 0x12);
    /// Menus and panels that sit above a widget.
    pub const ELEVATED: Rgb = (0x18, 0x18, 0x18);
    /// Hover fills, and the ring around a widget.
    pub const RAISED: Rgb = (0x1F, 0x1F, 0x1F);
}

/// Hairlines. `FAINT` is the one that separates a header from its body;
/// `SOFT` is the ring around a widget.
pub mod hairline {
    use super::{monochrome, Rgb};
    pub const FAINT: Rgb = (0x2A, 0x2A, 0x2E);
    pub const SOFT: Rgb = monochrome::RAISED;
    pub const ACTIVE: Rgb = monochrome::RAISED;
}

pub mod text {
    use super::Rgb;
    /// Graysite `--color-paper` — the warm off-white its pages read in.
    /// Pure white reads harsh against the ink ramp; the fixture test does
    /// not pin this token.
    pub const NORMAL: Rgb = (0xEC, 0xEA, 0xE7);
    pub const DIM: Rgb = (0xB9, 0xB9, 0xBE);
    pub const FAINT: Rgb = (0xA9, 0xA9, 0xB0);
}

/// The one hue — graysite's sodium-amber "sand" ramp plus its signal red.
/// The ink greys carry the surfaces; sand is reserved for accents that
/// must read as *alive*: selections, the focused card, active marks.
pub mod accent {
    use super::Rgb;
    /// sand-400 `#d4a373` — selection fills, focus outlines, active tab.
    pub const SAND: Rgb = (0xD4, 0xA3, 0x73);
    /// sand-300 `#dfb891` — brighter accent text (code chips, highlights).
    pub const SAND_HI: Rgb = (0xDF, 0xB8, 0x91);
    /// sand-200 `#e8cdb0` — the sheen's brightest stop.
    pub const SAND_LO: Rgb = (0xE8, 0xCD, 0xB0);
    /// signal `#b7513f` — destructive accents where DANGER is too pink.
    pub const SIGNAL: Rgb = (0xB7, 0x51, 0x3F);
}

pub mod status {
    use super::Rgb;
    pub const DANGER: Rgb = (0xE7, 0xA1, 0xA1);
    pub const OK: Rgb = (0x6F, 0xD3, 0x9A);
    /// StatusBar's CPU readout in the Electron shell: `#7aa2f7` under 50%,
    /// `#e6c07b` to 80%, `danger` above.
    pub const INFO: Rgb = (0x7A, 0xA2, 0xF7);
    pub const WARN: Rgb = (0xE6, 0xC0, 0x7B);
}

/// Pack an (r,g,b) triple into the u32 gpui::rgb() takes.
pub const fn hex(color: Rgb) -> u32 {
    (color.0 as u32) << 16 | (color.1 as u32) << 8 | color.2 as u32
}

pub mod geometry {
    pub const WIDGET_RADIUS: f32 = 0.0;
    /// Widget header height, matching `h-[34px]`.
    pub const HEADER_HEIGHT: f32 = 34.0;
    /// The ring around a widget is one *device* pixel in the DOM; at canvas
    /// zoom it stays one logical pixel so it does not thicken when zoomed.
    pub const HAIRLINE: f32 = 1.0;
    pub const RAIL_WIDTH: f32 = 56.0;
    pub const SIDEBAR_EXPANDED: f32 = 200.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vendored from the old Electron renderer's `tokens.ts`; kept as a fixture
    /// so the Electron tree could be deleted without losing the parity check.
    const TOKENS_SOURCE: &str = include_str!("../tests/fixtures/tokens.ts");
    /// Vendored from the old renderer's `WidgetFrame.tsx`; only the header
    /// height (`h-[34px]`) is contract.
    const WIDGET_FRAME_SOURCE: &str = include_str!("../tests/fixtures/WidgetFrame.tsx");

    fn hex(color: Rgb) -> String {
        format!("#{:02x}{:02x}{:02x}", color.0, color.1, color.2)
    }

    /// The native app must not drift into its own palette. Every colour here is
    /// checked against the file the renderer reads, so a change in either place
    /// fails a test rather than producing two Slates that look different.
    #[test]
    fn the_palette_matches_the_renderers_tokens() {
        let lowered = TOKENS_SOURCE.to_lowercase();

        for (name, color) in [
            ("base", monochrome::BASE),
            ("terminalHeader", monochrome::TERMINAL_HEADER),
            ("surface", monochrome::SURFACE),
            ("elevated", monochrome::ELEVATED),
            ("raised", monochrome::RAISED),
            ("hairline faint", hairline::FAINT),
            ("text dim", text::DIM),
            ("text faint", text::FAINT),
            ("danger", status::DANGER),
            ("ok", status::OK),
        ] {
            assert!(
                lowered.contains(&hex(color)),
                "{name} is {} here but that value is not in tokens.ts",
                hex(color)
            );
        }
    }

    #[test]
    fn the_rail_and_sidebar_widths_match_the_tokens() {
        assert!(TOKENS_SOURCE.contains("railWidth: '56px'"));
        assert!(TOKENS_SOURCE.contains("sidebarExpanded: '200px'"));
    }

    #[test]
    fn the_widget_chrome_matches_the_frame_component() {
        assert_eq!(geometry::WIDGET_RADIUS, 0.0);
        assert!(
            WIDGET_FRAME_SOURCE.contains("h-[34px]"),
            "the header height moved; this draws {}",
            geometry::HEADER_HEIGHT
        );
    }

    /// The chrome is hueless on purpose: coloured terminal output has to be the
    /// only colour on screen.
    #[test]
    fn the_greys_are_actually_grey() {
        for color in [
            monochrome::BASE,
            monochrome::TERMINAL_HEADER,
            monochrome::SURFACE,
            monochrome::ELEVATED,
            monochrome::RAISED,
        ] {
            assert_eq!(color.0, color.1, "{color:?} has a tint");
            assert_eq!(color.1, color.2, "{color:?} has a tint");
        }
    }

    #[test]
    fn the_greys_ascend() {
        let ladder = [
            monochrome::BASE,
            monochrome::TERMINAL_HEADER,
            monochrome::SURFACE,
            monochrome::ELEVATED,
            monochrome::RAISED,
        ];
        for pair in ladder.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "the surface ladder must get lighter, not darker: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }
}
