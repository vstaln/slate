// SPDX-License-Identifier: GPL-3.0-or-later
//! Embedded UI assets (vendored Lucide icons) served through gpui's AssetSource.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString, Styled};

const ICONS: &[(&str, &[u8])] = &[
    ("icons/arrow-left.svg", include_bytes!("../assets/icons/arrow-left.svg")),
    ("icons/arrow-right.svg", include_bytes!("../assets/icons/arrow-right.svg")),
    ("icons/check.svg", include_bytes!("../assets/icons/check.svg")),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    ("icons/cpu.svg", include_bytes!("../assets/icons/cpu.svg")),
    ("icons/ellipsis.svg", include_bytes!("../assets/icons/ellipsis.svg")),
    ("icons/eraser.svg", include_bytes!("../assets/icons/eraser.svg")),
    ("icons/expand.svg", include_bytes!("../assets/icons/expand.svg")),
    ("icons/eye.svg", include_bytes!("../assets/icons/eye.svg")),
    ("icons/eye-off.svg", include_bytes!("../assets/icons/eye-off.svg")),
    ("icons/folder-open.svg", include_bytes!("../assets/icons/folder-open.svg")),
    ("icons/git-branch.svg", include_bytes!("../assets/icons/git-branch.svg")),
    ("icons/hand.svg", include_bytes!("../assets/icons/hand.svg")),
    ("icons/layout-grid.svg", include_bytes!("../assets/icons/layout-grid.svg")),
    ("icons/maximize-2.svg", include_bytes!("../assets/icons/maximize-2.svg")),
    ("icons/minimize-2.svg", include_bytes!("../assets/icons/minimize-2.svg")),
    ("icons/minus.svg", include_bytes!("../assets/icons/minus.svg")),
    ("icons/mouse-pointer-2.svg", include_bytes!("../assets/icons/mouse-pointer-2.svg")),
    ("icons/pencil.svg", include_bytes!("../assets/icons/pencil.svg")),
    ("icons/pencil-thin.svg", include_bytes!("../assets/icons/pencil-thin.svg")),
    ("icons/settings.svg", include_bytes!("../assets/icons/settings.svg")),
    ("icons/square.svg", include_bytes!("../assets/icons/square.svg")),
    ("icons/trash-2.svg", include_bytes!("../assets/icons/trash-2.svg")),
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
