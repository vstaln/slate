//! TitleBar — the shell's fixed 40px top strip (`title-bar-shell`).
//!
//! In the shipped bundle (`index-*.js`, the `TitleBar` component) the bar
//! was a three-column grid — `minmax(0,1fr) auto minmax(0,1fr)`, `h-10`
//! with `pl-2 pr-0`, `Te.surface` over a `Te.raised` bottom hairline —
//! holding:
//!
//! - **left**: the canvas-history pill (`title-bar-history-switch`:
//!   Undo/Redo, disabled as `text-white/25`), a code-view-only sidebar
//!   toggle inside `title-bar-view-switch`, a transient
//!   `orcspace:title-flash` status text (`role="status"`, 13px medium),
//!   then a `WebkitAppRegion: drag` spacer;
//! - **center**: the Workspace View `tablist` — Canvas | Code | Overview;
//! - **right**: another drag spacer, "Flip terminals" (canvas/code only),
//!   the Arrange menu, the Git pill (`title-bar-git`: branch + dirty dot
//!   + checkout panel), an `h-4 w-px` divider, and — non-macOS only, the
//!     `!vt` guard — minimize, maximize/restore and close, each a `jr`
//!     button (`h-10 w-[46px]`, close hovering `#e04343` → white).
//!
//! This port keeps the grid, the surface/hairline chrome, the divider and
//! the window controls, and now backs most of the rest:
//!
//! - **Undo/Redo** — the shell's arrows dispatched `orcspace:canvas-undo`
//!   /`-redo`; here the `undo`/`redo` commands pop `CanvasView`'s
//!   `undo_stack`/`redo_stack` directly, dimmed to white/25 when empty.
//! - **View tabs** — the centre pill is the real `tablist` again. Slate
//!   renders Canvas only, so it sits active while Code and Overview wear
//!   the disabled white/25; a click flashes "isn't in this build" through
//!   the same notice path instead of switching.
//! - **Flip terminals** — `orcspace-flip-terminals`, persisted on
//!   `UiPreferences::terminals_flipped`. Terminal bodies honour it with
//!   the flip card (`terminal-flip-card`) in `canvas_view::widget_body`.
//! - **Git pill** — `read_git_chip` runs the same porcelain read the
//!   `/git/status` route does (`http::git_status`), on the background
//!   executor every `GIT_POLL_EVERY` like the shell's 8s focused poll.
//!   The checkout panel isn't ported; a click repolls and flashes the
//!   branch/dirty summary the original showed as a tooltip.
//! - **Sidebar toggle** — kept in the left cluster where the shell's
//!   code-view toggle sat; it's the only left chrome with a command.
//! - **Arrange** — already ported, as the status bar's `arrange:` chip.
//! - **Workspace name** — the `role="status"` slot's stand-in: the
//!   original's transient flash text was the only left-side text, so the
//!   folder name rides there as the title a native title bar shows.
//! - **presence** — the count is `snapshots.len()` (live terminals); the
//!   original's closest kin was the git dirty dot, so this renders the
//!   same way — a status dot + dim count — and hides at `0`.
//!
//! Every control dispatches through `CanvasView::titlebar_command`
//! (`cx.listener`, `json!` payloads): "undo"/"redo" pop the history
//! rings, "view" is a canvas-only no-op that flashes for the absent
//! views, "flip" toggles `prefs.terminals_flipped`, "git" repolls the
//! chip early, "sidebar" toggles `prefs.sidebar_open`, "settings" opens
//! the settings modal, "minimize" → `window.minimize_window()`,
//! "maximize" → `window.zoom_window()` — a toggle, the original's
//! `toggleMaximize` — and "close" → `cx.quit()`. The controls row is
//! compiled out on macOS, the original's `!vt` guard (the traffic lights
//! own that corner there, and the bar's left padding grows to 78px to
//! clear them). Gpui always answers minimize/zoom/close on Linux; a
//! platform that can't can gate inside `titlebar_command` via
//! `window.window_controls()`.
//!
//! The spacers and the workspace title are the `WebkitAppRegion: drag`
//! areas: press calls `window.start_window_move()` (the
//! `_NET_WM_MOVERESIZE` / `xdg_toplevel.move` path), right-press opens
//! the native window menu where the platform has one. Every handler
//! stops propagation — a press on the bar must never reach the canvas's
//! pan/draw handlers.

use gpui::*;
use serde_json::json;
#[cfg(not(target_os = "macos"))]
use slate_app::canvas::TITLE_BAR_HEIGHT;
use slate_app::theme;

/// `jr` — a window control is the full bar height and 46px wide.
#[cfg(not(target_os = "macos"))]
const CONTROL_W: f32 = 46.0;
/// The Close hover — the shell's literal `#e04343`, never a theme token.
#[cfg(not(target_os = "macos"))]
const CLOSE_HOVER: u32 = 0xE04343;
/// `en` — the bordered pill wrap: `h-[30px] … p-[3px]` on `Te.base`.
const PILL_H: f32 = 30.0;
/// `Ft` — pill buttons were `h-[24px]` inside the wrap.
const BUTTON_H: f32 = 24.0;
/// The shell's disabled-button colour, `text-white/25`.
const DISABLED_TEXT: u32 = 0xFFFFFF40;
/// The git chip's refresh cadence — the shell polled `git.status()` every
/// 8s while focused (30s while hidden; the native window doesn't track
/// document focus, so the focused rate is kept).
pub const GIT_POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(8);

/// The right-cluster git chip's state — a trimmed `git status` read: the
/// `##` header's branch (the original's `gitStatus.branch`, `HEAD` when
/// detached) and the dirty count it summed over modified+untracked+
/// staged+conflicted — which is just the porcelain row count.
#[derive(Clone, Debug, Default)]
pub struct GitChip {
    /// The workspace answered `git status` — it is a repository.
    pub repo: bool,
    /// `##` header name before `...upstream`; "HEAD" when detached, like
    /// the chip's `gitStatus.branch || 'HEAD'` fallback.
    pub branch: String,
    /// Porcelain row count — the shell's `dirtyCount` over all buckets.
    pub dirty: u32,
}

/// `readGitStatus` reduced to what the chip draws: the same
/// `git -C <dir> status --porcelain=v1 -b -z` the `/git/status` route
/// runs (`http::git_status`), parsed the same way — `##` header for the
/// branch, one dirty count per porcelain row, renames/copies counting
/// once. Never throws: a non-repo, a missing git and a failed read all
/// land as `repo: false`, the shell's EMPTY status. Run off the UI
/// thread — this is a subprocess.
pub fn read_git_chip(dir: &std::path::Path) -> GitChip {
    let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "status",
            "--porcelain=v1",
            "-b",
            "-z",
            "--untracked-files=normal",
        ])
        .output()
    else {
        return GitChip::default();
    };
    if !output.status.success() {
        return GitChip::default();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let normalized = text.replace("\r\0", "\0").replace("\r\n", "\n");
    let entries: Vec<&str> = normalized
        .split('\0')
        .filter(|line| !line.is_empty())
        .collect();
    // `parseHeader` — "## main...origin/main [ahead 1, behind 2]", or
    // "## HEAD (no branch)" detached, or "## No commits yet on main".
    let header = entries
        .first()
        .filter(|line| line.starts_with("##"))
        .copied()
        .unwrap_or_default();
    let body = header.get(2..).unwrap_or("").trim();
    let branches = body.split_once(" [").map(|(a, _)| a).unwrap_or(body);
    let name = branches
        .split_once("...")
        .map(|(a, _)| a)
        .unwrap_or(branches)
        .replace("No commits yet on ", "");
    let branch = if name.starts_with("HEAD") {
        "HEAD".to_owned()
    } else {
        name
    };
    let mut dirty = 0u32;
    let mut iter = entries
        .iter()
        .skip(if header.is_empty() { 0 } else { 1 })
        .peekable();
    while let Some(entry) = iter.next() {
        if entry.len() < 2 {
            continue;
        }
        dirty += 1;
        // Renames and copies carry a second NUL-separated path that is
        // not a second changed file.
        if entry.starts_with('R') || entry.starts_with('C') {
            iter.next();
        }
    }
    GitChip {
        repo: !header.is_empty(),
        branch,
        dirty,
    }
}

/// What the bar needs from `CanvasView` — same role as
/// `views_toolbar::ToolbarState`: plain data in, one element out.
pub struct TitlebarState<'a> {
    /// `workspace::current()` — its basename fills the title slot.
    pub workspace_dir: &'a std::path::Path,
    /// Live terminal count for the presence badge; `0` hides it.
    pub presence_count: usize,
    /// `undo_stack`/`redo_stack` non-empty — the history arrows' enabled.
    pub can_undo: bool,
    pub can_redo: bool,
    /// `orcspace-flip-terminals` — paints the toggle's active state.
    pub terminals_flipped: bool,
    /// The last lazy poll's repo read (`CanvasView::git_chip`).
    pub git: &'a GitChip,
}

/// `WebkitAppRegion: drag` — dead space that moves the window. A left
/// press hands the drag to the compositor; a right press asks for the
/// platform's title-bar menu (`window_controls().window_menu` gates it —
/// X11 reports one, minimal Wayland compositors may not).
fn drag_space(cx: &mut Context<crate::canvas_view::CanvasView>) -> Div {
    div()
        .h_full()
        .flex_1()
        .min_w(px(8.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                window.start_window_move();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|_this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if window.window_controls().window_menu {
                    window.show_window_menu(event.position);
                }
            }),
        )
}

/// `en` — the bordered pill wrap: `h-[30px] rounded-pill border-bg-raise
/// bg-bg p-[3px]`. The history switch, the sidebar toggle, the view tabs,
/// flip and git all sit inside one.
fn pill_wrap() -> Div {
    div()
        .h(px(PILL_H))
        .flex()
        .flex_row()
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(rgb(theme::hex(theme::monochrome::RAISED)))
        .bg(rgb(theme::hex(theme::monochrome::BASE)))
        .px(px(3.0))
        .flex_none()
}

/// `Ft` — the bar's pill button: 24px tall, rounded, dim until hover.
/// One `action` verb, dispatched with the contract's empty payload.
fn action_button(
    glyph: &'static str,
    action: &'static str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    div()
        .h(px(BUTTON_H))
        .min_w(px(30.0))
        .px_2()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .cursor_pointer()
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(glyph)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                this.titlebar_command(action, json!({}), window, cx);
            }),
        )
}

/// `Ft` — a history arrow (`title-bar-history-switch`'s Undo/Redo):
/// the pill button while the stack has entries, the shell's
/// `cursor-default text-white/25` and no click when it's empty.
fn history_button(
    glyph: &'static str,
    action: &'static str,
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    let mut el = div()
        .h(px(BUTTON_H))
        .min_w(px(30.0))
        .px_2()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .child(glyph);
    if enabled {
        el = el
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.titlebar_command(action, json!({}), window, cx);
                }),
            );
    } else {
        el = el.text_color(rgba(DISABLED_TEXT));
    }
    el
}

/// `Ft` — a Workspace View tab (`title-bar-view-tab`): the active view
/// wears `bg-bg-hover text-white font-semibold`, a live inactive one
/// `text-white/60` with the hover pair, and a view this build doesn't
/// render the disabled white/25. The click still dispatches "view" —
/// canvas is a no-op and the absent views flash the notice, the shell's
/// `title-flash` stand-in.
fn view_tab(
    label: &'static str,
    view: &'static str,
    active: bool,
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    let mut el = div()
        .h(px(BUTTON_H))
        .flex_none()
        .px(px(11.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .child(label);
    if active {
        el = el
            .bg(rgb(theme::hex(theme::monochrome::RAISED)))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme::hex(theme::text::NORMAL)));
    } else if enabled {
        el = el
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            });
    } else {
        el = el.text_color(rgba(DISABLED_TEXT));
    }
    el.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, window, cx| {
            cx.stop_propagation();
            this.titlebar_command("view", json!({ "view": view }), window, cx);
        }),
    )
}

/// The "Flip terminals" toggle (`title-bar-flip-terminals`): `VIEW_TAB`
/// with the eye glyph and the state-flipping label — EyeOff "Flip
/// terminals" at rest, Eye "Show terminals" while flipped — and the
/// active paint the tabs use.
fn flip_button(flipped: bool, cx: &mut Context<crate::canvas_view::CanvasView>) -> Div {
    let mut el = div()
        .h(px(BUTTON_H))
        .flex_none()
        .px(px(9.0))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .rounded_full()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::hex(if flipped {
                    theme::text::NORMAL
                } else {
                    theme::text::FAINT
                })))
                .child(if flipped { "◉" } else { "◎" }),
        )
        .child(if flipped {
            "Show terminals"
        } else {
            "Flip terminals"
        });
    if flipped {
        el = el
            .bg(rgb(theme::hex(theme::monochrome::RAISED)))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme::hex(theme::text::NORMAL)));
    } else {
        el = el
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            });
    }
    el.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, window, cx| {
            cx.stop_propagation();
            this.titlebar_command("flip", json!({}), window, cx);
        }),
    )
}

/// The git pill (`title-bar-git`): `⎇` + branch + dirty dot inside the
/// same bordered wrap. The glyph is accent while the tree is dirty (the
/// native accent is `status::INFO`), dim on a clean repo, faint off one.
/// No checkout panel in this build — a click repolls and flashes the
/// summary the original's tooltip carried.
fn git_button(chip: &GitChip, cx: &mut Context<crate::canvas_view::CanvasView>) -> Div {
    let (glyph_color, label) = if chip.repo {
        (
            theme::hex(if chip.dirty > 0 {
                theme::status::INFO
            } else {
                theme::text::DIM
            }),
            if chip.branch.is_empty() {
                "HEAD".to_owned()
            } else {
                chip.branch.clone()
            },
        )
    } else {
        (theme::hex(theme::text::FAINT), "Git".to_owned())
    };
    let mut el = div()
        .h(px(BUTTON_H))
        .flex_none()
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded_full()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .cursor_pointer()
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(div().flex_none().text_color(rgb(glyph_color)).child("⎇"))
        .child(
            div()
                .flex_none()
                .max_w(px(110.0))
                .truncate()
                .child(label),
        );
    if chip.dirty > 0 {
        el = el.child(
            div()
                .w(px(6.0))
                .h(px(6.0))
                .flex_none()
                .rounded_full()
                .bg(rgb(theme::hex(theme::status::INFO))),
        );
    }
    el.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, window, cx| {
            cx.stop_propagation();
            this.titlebar_command("git", json!({}), window, cx);
        }),
    )
}

/// `jr` — a window-control cell: `h-10 w-[46px]`, glyph centred, the
/// faint-on-raised hover; `danger` paints the shell's red close hover.
#[cfg(not(target_os = "macos"))]
fn window_button(
    glyph: &'static str,
    action: &'static str,
    danger: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    div()
        .h(px(TITLE_BAR_HEIGHT))
        .w(px(CONTROL_W))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.0))
        .text_color(rgb(theme::hex(theme::text::FAINT)))
        .cursor_pointer()
        .hover(move |el| {
            if danger {
                el.bg(rgb(CLOSE_HOVER)).text_color(rgb(0xFFFFFF))
            } else {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            }
        })
        .child(glyph)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                this.titlebar_command(action, json!({}), window, cx);
            }),
        )
}

/// The bar. `state.workspace_dir`'s basename is the left title text (the
/// full path only standing in when the basename is empty, e.g. `/`);
/// everything else the shell drew is covered in the module doc.
pub fn titlebar_pane(
    state: &TitlebarState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let workspace_name = state
        .workspace_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            let full = state.workspace_dir.to_string_lossy().into_owned();
            if full.is_empty() {
                "Slate".to_owned()
            } else {
                full
            }
        });

    // `title-bar-shell`: grid 1fr|auto|1fr → flex with equal flex-1
    // sides, which centres the middle column the same way. `pl-2`, macOS
    // grows it to 78px for the traffic lights (the bundle's `vt` branch).
    let bar = div()
        .size_full()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
        .border_b_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .pl(px(if cfg!(target_os = "macos") { 78.0 } else { 8.0 }))
        // A press anywhere on the strip that a child didn't claim never
        // reaches the canvas's pan/draw/select handlers.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event, _window, cx| {
                cx.stop_propagation();
            }),
        );

    // Left: `title-bar-history-switch` (Undo/Redo) then the sidebar
    // toggle in the code-view pill's slot, the `role="status"` title
    // text, and the drag spacer.
    let left = div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            pill_wrap()
                .child(history_button("←", "undo", state.can_undo, cx))
                .child(history_button("→", "redo", state.can_redo, cx)),
        )
        .child(pill_wrap().child(action_button("◧", "sidebar", cx)))
        .child(
            // `role="status"` — the flash slot. 13px medium, and it drags
            // like a native title bar.
            div()
                .min_w_0()
                .flex_none()
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .truncate()
                .child(workspace_name)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_this, _event, window, cx| {
                        cx.stop_propagation();
                        window.start_window_move();
                    }),
                ),
        )
        .child(drag_space(cx));

    // Centre: the Workspace View `tablist` — Canvas active; Code and
    // Overview carry the disabled paint until those views exist.
    let center = div()
        .flex_none()
        .min_w_0()
        .flex()
        .items_center()
        .child(
            pill_wrap()
                .child(view_tab("Canvas", "canvas", true, true, cx))
                .child(view_tab("Code", "code", false, false, cx))
                .child(view_tab("Overview", "overview", false, false, cx)),
        );

    // Right: drag space, live count, the flip toggle, the git pill,
    // settings, divider, window controls — the shell's island order.
    let mut right = div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .child(drag_space(cx));

    if state.presence_count > 0 {
        right = right.child(
            div()
                .flex_none()
                .h(px(BUTTON_H))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(rgb(theme::hex(theme::status::OK))),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!("{} live", state.presence_count)),
                ),
        );
    }

    right = right
        .child(
            div()
                .flex_none()
                .mx(px(4.0))
                .child(pill_wrap().child(flip_button(state.terminals_flipped, cx))),
        )
        .child(
            div()
                .flex_none()
                .mx(px(4.0))
                .child(pill_wrap().child(git_button(state.git, cx))),
        )
        .child(action_button("⚙", "settings", cx));

    // `!vt` — window controls exist only off macOS, flush to the right
    // edge (`pr-0`). Separator: `mx-0.5 h-4 w-px bg-line-soft/80`.
    #[cfg(not(target_os = "macos"))]
    {
        right = right
            .child(
                div()
                    .mx(px(2.0))
                    .h(px(16.0))
                    .w(px(1.0))
                    .flex_none()
                    .bg(rgba((theme::hex(theme::hairline::SOFT) << 8) | 0xCC)),
            )
            .child(window_button("–", "minimize", false, cx))
            .child(window_button("□", "maximize", false, cx))
            .child(window_button("×", "close", true, cx));
    }

    bar.child(left).child(center).child(right)
}
