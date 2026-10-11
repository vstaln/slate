// SPDX-License-Identifier: GPL-3.0-or-later
//! Embedded UI assets (vendored Lucide icons) served through gpui's AssetSource.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString, Styled};

const ICONS: &[(&str, &[u8])] = &[
    // Agent brand marks — the shell's `agent-icons.tsx` paths verbatim
    // (Claude keeps its brand fill; the rest ride `currentColor` so the
    // caller's `text_color` paints them).
    (
        "icons/agent-antigravity.svg",
        include_bytes!("../assets/icons/agent-antigravity.svg"),
    ),
    (
        "icons/agent-claude.svg",
        include_bytes!("../assets/icons/agent-claude.svg"),
    ),
    (
        "icons/agent-codex.svg",
        include_bytes!("../assets/icons/agent-codex.svg"),
    ),
    (
        "icons/agent-cursor.svg",
        include_bytes!("../assets/icons/agent-cursor.svg"),
    ),
    (
        "icons/agent-gray.svg",
        include_bytes!("../assets/icons/agent-gray.svg"),
    ),
    (
        "icons/agent-grok.svg",
        include_bytes!("../assets/icons/agent-grok.svg"),
    ),
    (
        "icons/agent-kimi.svg",
        include_bytes!("../assets/icons/agent-kimi.svg"),
    ),
    (
        "icons/agent-opencode.svg",
        include_bytes!("../assets/icons/agent-opencode.svg"),
    ),
    (
        "icons/arrow-left.svg",
        include_bytes!("../assets/icons/arrow-left.svg"),
    ),
    (
        "icons/arrow-right.svg",
        include_bytes!("../assets/icons/arrow-right.svg"),
    ),
    (
        "icons/check.svg",
        include_bytes!("../assets/icons/check.svg"),
    ),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    ("icons/cpu.svg", include_bytes!("../assets/icons/cpu.svg")),
    (
        "icons/ellipsis.svg",
        include_bytes!("../assets/icons/ellipsis.svg"),
    ),
    (
        "icons/eraser.svg",
        include_bytes!("../assets/icons/eraser.svg"),
    ),
    (
        "icons/expand.svg",
        include_bytes!("../assets/icons/expand.svg"),
    ),
    ("icons/eye.svg", include_bytes!("../assets/icons/eye.svg")),
    (
        "icons/eye-off.svg",
        include_bytes!("../assets/icons/eye-off.svg"),
    ),
    (
        "icons/folder-open.svg",
        include_bytes!("../assets/icons/folder-open.svg"),
    ),
    (
        "icons/git-branch.svg",
        include_bytes!("../assets/icons/git-branch.svg"),
    ),
    (
        "icons/globe.svg",
        include_bytes!("../assets/icons/globe.svg"),
    ),
    ("icons/hand.svg", include_bytes!("../assets/icons/hand.svg")),
    (
        "icons/history.svg",
        include_bytes!("../assets/icons/history.svg"),
    ),
    (
        "icons/layout-grid.svg",
        include_bytes!("../assets/icons/layout-grid.svg"),
    ),
    (
        "icons/maximize-2.svg",
        include_bytes!("../assets/icons/maximize-2.svg"),
    ),
    (
        "icons/minimize-2.svg",
        include_bytes!("../assets/icons/minimize-2.svg"),
    ),
    (
        "icons/minus.svg",
        include_bytes!("../assets/icons/minus.svg"),
    ),
    (
        "icons/mouse-pointer-2.svg",
        include_bytes!("../assets/icons/mouse-pointer-2.svg"),
    ),
    (
        "icons/panel-left.svg",
        include_bytes!("../assets/icons/panel-left.svg"),
    ),
    (
        "icons/pencil.svg",
        include_bytes!("../assets/icons/pencil.svg"),
    ),
    (
        "icons/pencil-thin.svg",
        include_bytes!("../assets/icons/pencil-thin.svg"),
    ),
    ("icons/pin.svg", include_bytes!("../assets/icons/pin.svg")),
    (
        "icons/refresh-cw.svg",
        include_bytes!("../assets/icons/refresh-cw.svg"),
    ),
    ("icons/play.svg", include_bytes!("../assets/icons/play.svg")),
    ("icons/plus.svg", include_bytes!("../assets/icons/plus.svg")),
    (
        "icons/settings.svg",
        include_bytes!("../assets/icons/settings.svg"),
    ),
    (
        "icons/square.svg",
        include_bytes!("../assets/icons/square.svg"),
    ),
    (
        "icons/terminal.svg",
        include_bytes!("../assets/icons/terminal.svg"),
    ),
    (
        "icons/trash-2.svg",
        include_bytes!("../assets/icons/trash-2.svg"),
    ),
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
];

/// A vendored Lucide icon (`icons/<name>.svg`) at a square `size`. gpui's
/// Svg paints from its own `text_color` — it does not inherit the ambient
/// text style — so the colour is passed explicitly (0xRRGGBB).
pub fn icon(name: &'static str, size: f32, color: u32) -> gpui::Svg {
    gpui::svg()
        .path(format!("icons/{name}.svg"))
        .w(gpui::px(size))
        .h(gpui::px(size))
        .text_color(gpui::rgb(color))
}

pub struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}
