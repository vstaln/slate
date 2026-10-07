//! Image pane — the read half of the old Electron `ImageWidget`.
//!
//! The original took the widget's `imagePath`/`imageName` payload fields —
//! the same fields `projection.rs` keeps on `Widget.image_path` /
//! `Widget.image_name` — fetched a data URL through `media.dataUrl`, and drew
//! it `object-contain`. The native port hands the path straight to gpui's
//! `img()` (its `ImageSource::Path` loader reads the file and decodes png /
//! jpg / gif / webp / svg / bmp / ico / qoi and friends) with
//! `ObjectFit::Contain`, plus the same two fallbacks the widget had: "could
//! not be loaded" when the file is there but broken, "No image attached"
//! when the payload never carried one.

use gpui::*;
use slate_app::theme;
use std::path::PathBuf;

/// The caption line under the image — the original showed `name || path ||
/// 'Clipboard image'`.
fn caption(path: Option<&str>, name: Option<&str>) -> String {
    name.or(path).unwrap_or("Clipboard image").to_owned()
}

/// The dim placeholder the original painted when there was nothing to draw.
fn placeholder(text: String, detail: Option<String>) -> AnyElement {
    let mut box_el = div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .size_full()
        .p_2()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(text),
        );
    if let Some(detail) = detail {
        box_el = box_el.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(detail),
        );
    }
    box_el.into_any_element()
}

pub fn image_pane(
    image_path: Option<&str>,
    image_name: Option<&str>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    // Read-only pane: the context is accepted for signature uniformity but
    // unused.
    let _ = cx;
    let body: AnyElement = match image_path {
        Some(path) if !path.trim().is_empty() => {
            let detail = path.to_owned();
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(PathBuf::from(path))
                        .size_full()
                        .object_fit(ObjectFit::Contain)
                        .with_fallback(move || {
                            placeholder(
                                "Image could not be loaded".to_owned(),
                                Some(detail.clone()),
                            )
                        }),
                )
                .into_any_element()
        }
        _ => placeholder("No image attached".to_owned(), None),
    };

    div()
        .flex()
        .flex_col()
        .size_full()
        .gap_1()
        .p_1()
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .bg(rgb(theme::hex(theme::monochrome::BASE)))
                .child(body),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(caption(image_path, image_name)),
        )
}
