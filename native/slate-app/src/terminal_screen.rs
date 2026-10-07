//! Terminal screen state plus the input side of the protocol.
//!
//! Output goes through [`TerminalScreen::process`]; queries the program asks
//! (DSR/CPR/DA, DECRPM mode reports, XTVERSION, XTGETTCAP, kitty flag query)
//! queue replies drained by [`TerminalScreen::take_replies`] — the engine
//! already writes those back to the PTY.
//!
//! Input APIs for the canvas/view layer — every one returns the bytes to
//! write to the PTY, `None`/`empty` meaning "the program didn't ask for
//! this":
//! - [`TerminalScreen::input`] — `KeyInput::Text` / `Paste` / `Key` /
//!   `KeyEvent` (press/repeat/release). Honors legacy encodings and the
//!   kitty keyboard progressive-enhancement stack.
//! - [`TerminalScreen::paste`] — raw paste text, bracketed when the program
//!   enabled DECSET 2004.
//! - [`TerminalScreen::mouse_event`] — cell-coordinate mouse reporting for
//!   DECSET 9/1000/1002/1003 tracking, encoded per 1005/1006/1015/1016.
//!   [`TerminalScreen::mouse_position_event`] is the pixel-geometry variant.
//! - [`TerminalScreen::focus_event`] — DECSET 1004 focus in/out reports.
//! - [`TerminalScreen::scroll`]/[`TerminalScreen::scroll_pixels`] — wheel
//!   scrolling; wheel events over a tracked mouse belong to `mouse_event`.

/// RGB triple for one terminal cell color. Same values as before;
/// the gpui view packs them with crate::theme::hex().
pub type CellRgb = (u8, u8, u8);

use crate::terminal_protocol::{Control, Decoder};
use std::time::{Duration, Instant};

pub struct TerminalScreen {
    parser: vt100::Parser,
    controls: Decoder,
    wheel_remainder: f32,
    focus_reporting: bool,
    mouse_mode: u16,
    mouse_encoding: MouseEncoding,
    mouse_buttons: [bool; 3],
    mouse_position: Option<(u16, u16)>,
    cursor_blink: bool,
    sync_started: Option<Instant>,
    sync_buffer: Vec<u8>,
    pending_controls: Vec<(usize, Control)>,
    replies: Vec<u8>,
    /// OSC 52 clipboard writes (base64 payloads) not yet drained by the view.
    clipboard_writes: Vec<String>,
    /// The last cwd the program reported through OSC 7.
    cwd: Option<String>,
    /// Kitty keyboard progressive-enhancement stack; the last entry is the
    /// current flag set. Never empty — the base entry is all flags off.
    kitty_stack: Vec<u8>,
    /// An active text selection (anchor + head), in history-absolute rows —
    /// see [`Selection`].
    selection: Option<Selection>,
}

/// A drag selection endpoint pair. Rows are stored *absolute*: row 0 is the
/// live grid's top row, negative rows reach into scrollback history. The
/// stored row is `visible_row - scrollback_offset` at anchor time, so the
/// highlight follows its content as the user scrolls instead of sticking
/// to screen coordinates (xterm anchors selections to the buffer, not the
/// viewport — same model). Columns are grid columns, 0-based.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: (i64, u16),
    pub head: (i64, u16),
}

impl Selection {
    /// The two ends in reading order (top-left → bottom-right).
    fn ordered(&self) -> ((i64, u16), (i64, u16)) {
        if (self.anchor.0, self.anchor.1) <= (self.head.0, self.head.1) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

// vt100::Parser's own Debug dumps the whole grid; the state struct that
// embeds this only needs to know a screen exists.
impl std::fmt::Debug for TerminalScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalScreen").finish_non_exhaustive()
    }
}

pub fn last_mode_toggle(data: &[u8], mode: u16) -> Option<bool> {
    Decoder::default()
        .advance(data)
        .into_iter()
        .filter_map(|control| match control {
            Control::Mode(number, enabled) if number == mode => Some(enabled),
            Control::Reset => Some(false),
            _ => None,
        })
        .next_back()
}

const MAX_SYNC_BYTES: usize = 1 << 20;
const MAX_SYNC_TIME: Duration = Duration::from_millis(150);
/// Kitty keyboard flag-stack depth — the fixed size ghostty uses; pushing
/// past it evicts the oldest entry instead of growing without bound.
const KITTY_STACK_MAX: usize = 8;
/// Kitty progressive-enhancement bits (kitty keyboard protocol spec).
const KITTY_DISAMBIGUATE: u8 = 0b1;
const KITTY_EVENT_TYPES: u8 = 0b10;
const KITTY_ALTERNATE_KEYS: u8 = 0b100;
const KITTY_ALL_KEYS: u8 = 0b1000;
const KITTY_ASSOCIATED_TEXT: u8 = 0b10000;

/// The coordinate-encoding half of mouse reporting, negotiated separately
/// from the tracking mode: xterm keeps these in one field where the last
/// DECSET wins. 1005/1006/1015/1016 select Utf8/Sgr/Urxvt/SgrPixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum MouseEncoding {
    /// `CSI M Cb Cx Cy` — one byte per field, 223-coordinate ceiling.
    #[default]
    Default,
    /// DECSET 1005: same layout, each field UTF-8 encoded so columns >223 fit.
    Utf8,
    /// DECSET 1006: `CSI <Cb;Cx;CyM/m` — release keeps its button.
    Sgr,
    /// DECSET 1015: `CSI Cb+32;Cx;CyM` — decimal params, no release button.
    Urxvt,
    /// DECSET 1016: SGR layout with pixel instead of cell coordinates.
    SgrPixels,
}

impl Default for TerminalScreen {
    fn default() -> Self {
        Self {
            // 1500 lines, the same cap the renderer sets on xterm: every open
            // terminal holds its own buffer, and a workspace full of agents is
            // the case this is built for.
            parser: vt100::Parser::new(32, 120, 1500),
            wheel_remainder: 0.0,
            focus_reporting: false,
            mouse_mode: 0,
            mouse_encoding: MouseEncoding::Default,
            mouse_buttons: [false; 3],
            mouse_position: None,
            cursor_blink: true,
            sync_started: None,
            sync_buffer: Vec::new(),
            controls: Decoder::default(),
            pending_controls: Vec::new(),
            replies: Vec::new(),
            clipboard_writes: Vec::new(),
            cwd: None,
            kitty_stack: vec![0],
            selection: None,
        }
    }
}

/// Own mouse button — replaces egui::PointerButton at the boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Primary = 0,
    Middle = 1,
    Secondary = 2,
}

/// Own mouse input — replaces egui::Event at the boundary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MouseInput {
    Press {
        button: MouseButton,
        pos: (f32, f32),
    },
    Release {
        button: MouseButton,
        pos: (f32, f32),
    },
    Move {
        pos: (f32, f32),
    },
}

/// Cell-space mouse event for `mouse_event`: what happened, with the cell
/// supplied separately. `Move(None)` is motion with no button held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseKind {
    Press(MouseButton),
    Release(MouseButton),
    Move(Option<MouseButton>),
    /// A wheel notch — always reported as a press; wheels have no release.
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

/// Key press phases for `KeyInput::KeyEvent`. Release/repeat only produce
/// bytes when the kitty event-types flag (0b10) is active; anything else
/// encodes release as silence and repeat as a second press.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

/// Own key enum — the egui::Key dependency died with the eframe UI.
/// Only the keys the terminal maps are listed; Char covers the rest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermKey {
    Char(char),
    Enter,
    Backspace,
    Escape,
    Tab,
    Delete,
    Insert,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Home,
    End,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Space,
    /// A keypad key that has a printable face — the digit keys and
    /// `.` `/` `*` `-` `+` `=` `,` — kept as the literal it types when
    /// application keypad mode is off.
    Numpad(char),
    /// Keypad Enter: `\x1bOM` in application mode, `\r` otherwise, 57414
    /// under the kitty protocol.
    NumpadEnter,
}

/// Own modifiers bit — replaces egui::Modifiers at the boundary.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct KeyMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub mac_cmd: bool,
}

/// One key press translated at the UI boundary into terminal bytes.
/// Text input arrives as Text/Paste; special keys as Key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeyInput {
    Text(String),
    Paste(String),
    Key {
        key: TermKey,
        modifiers: KeyMods,
    },
    /// `Key` plus the event phase. `Key` is a press; this variant exists so
    /// repeat/release reach the PTY when the kitty event-types flag is up.
    KeyEvent {
        key: TermKey,
        modifiers: KeyMods,
        kind: KeyEventKind,
    },
}

impl TerminalScreen {
    /// The box one cell occupies: the monospace advance, and the font's own
    /// row height rather than a multiple of the point size. Box-drawing
    /// characters only join up on a grid matching the metrics the font was
    /// designed around — the renderer sets xterm's `lineHeight: 1` for the
    /// same reason, and a 1.25 multiplier left gaps in every frame a TUI drew.
    /// Feed raw pty bytes in. Synchronized-update frames (DECSET 2026)
    /// buffer until the closing sequence so a TUI frame reaches the screen
    /// whole or not at all; everything else flushes straight through.
    pub fn process(&mut self, bytes: &[u8]) {
        self.flush_expired();
        for byte in bytes {
            self.sync_buffer.push(*byte);
            for control in self.controls.advance(std::slice::from_ref(byte)) {
                match control {
                    Control::Mode(2026, true) => {
                        if self.sync_started.is_none() {
                            self.flush_frame();
                            self.sync_started = Some(Instant::now());
                        }
                    }
                    Control::Mode(2026, false) => {
                        self.sync_started = None;
                        self.flush_frame();
                    }
                    _ => self
                        .pending_controls
                        .push((self.sync_buffer.len(), control)),
                }
            }
            if self.sync_buffer.len() >= MAX_SYNC_BYTES {
                self.sync_started = None;
                self.flush_frame();
            }
        }
        if self.sync_started.is_none() {
            self.flush_frame();
        }
    }

    pub fn flush_expired(&mut self) {
        if self
            .sync_started
            .is_some_and(|started| started.elapsed() >= MAX_SYNC_TIME)
        {
            self.sync_started = None;
            self.flush_frame();
        }
    }

    fn flush_frame(&mut self) {
        let frame = std::mem::take(&mut self.sync_buffer);
        let mut offset = 0;
        for (end, control) in std::mem::take(&mut self.pending_controls) {
            self.parser.process(&frame[offset..end]);
            offset = end;
            match control {
                Control::Mode(12, enabled) => self.cursor_blink = enabled,
                // 9 is the X10 mode (press only); 1000/1002/1003 widen to
                // release, button-motion, any-motion tracking.
                Control::Mode(mode @ (9 | 1000 | 1002 | 1003), enabled) => {
                    if enabled {
                        self.mouse_mode = mode;
                    } else if self.mouse_mode == mode {
                        self.mouse_mode = 0;
                    }
                    self.mouse_buttons = [false; 3];
                    self.mouse_position = None;
                }
                Control::Mode(1004, enabled) => self.focus_reporting = enabled,
                // The four encodings share xterm's extend-coords field, so
                // the last DECSET wins; resetting only clears when it names
                // the encoding in use.
                Control::Mode(mode @ (1005 | 1006 | 1015 | 1016), enabled) => {
                    let encoding = match mode {
                        1005 => MouseEncoding::Utf8,
                        1006 => MouseEncoding::Sgr,
                        1015 => MouseEncoding::Urxvt,
                        _ => MouseEncoding::SgrPixels,
                    };
                    if enabled {
                        self.mouse_encoding = encoding;
                    } else if self.mouse_encoding == encoding {
                        self.mouse_encoding = MouseEncoding::Default;
                    }
                }
                Control::Cursor(style) => {
                    self.cursor_blink = style == 0 || style % 2 == 1;
                }
                Control::Reset => {
                    self.cursor_blink = true;
                    self.focus_reporting = false;
                    self.mouse_mode = 0;
                    self.mouse_encoding = MouseEncoding::Default;
                    self.mouse_buttons = [false; 3];
                    self.mouse_position = None;
                    self.kitty_stack.clear();
                    self.kitty_stack.push(0);
                }
                // DECRPM `CSI ? Ps $ p`: report what we actually track —
                // 0 = mode we don't recognize, 1 = set, 2 = reset.
                Control::ModeReport(mode) => {
                    let state = self.mode_state(mode);
                    self.replies
                        .extend_from_slice(format!("\x1b[?{mode};{state}$y").as_bytes());
                }
                // XTVERSION `CSI > q`: DCS > | name ST.
                Control::VersionReport => {
                    self.replies.extend_from_slice(b"\x1bP>|slate(1)\x1b\\");
                }
                Control::TcapQuery(payload) => self.tcap_reply(&payload),
                Control::Status(5, false) => self.replies.extend_from_slice(b"\x1b[0n"),
                Control::Status(6, private) => {
                    let (row, col) = self.screen().cursor_position();
                    let prefix = if private { "?" } else { "" };
                    self.replies.extend_from_slice(
                        format!("\x1b[{prefix}{};{}R", row + 1, col + 1).as_bytes(),
                    );
                }
                Control::Attributes => self.replies.extend_from_slice(b"\x1b[?1;2c"),
                Control::Clipboard(payload) => self.clipboard_writes.push(payload),
                Control::WorkingDirectory(path) => self.cwd = Some(path),
                // Kitty keyboard stack: pushing with a full stack evicts the
                // oldest entry, and popping the last entry resets the flags —
                // the spec's answer to a program that never balances.
                Control::KittyPush(flags) => {
                    if self.kitty_stack.len() >= KITTY_STACK_MAX {
                        self.kitty_stack.remove(0);
                    }
                    self.kitty_stack.push(flags);
                }
                Control::KittyPop(count) => {
                    // A pop that empties the stack resets every flag; the
                    // base entry stays so the stack is never bare.
                    let remaining = self.kitty_stack.len().saturating_sub(count as usize);
                    self.kitty_stack.truncate(remaining.max(1));
                    if remaining == 0 {
                        self.kitty_stack[0] = 0;
                    }
                }
                Control::KittyQuery => {
                    // The answer is the same shape as the query, carrying the
                    // flags currently in effect.
                    let flags = self.kitty_flags();
                    self.replies
                        .extend_from_slice(format!("\x1b[?{flags}u").as_bytes());
                }
                Control::KittySet(flags, mode) => {
                    if let Some(current) = self.kitty_stack.last_mut() {
                        match mode {
                            1 => *current = flags,
                            2 => *current |= flags,
                            3 => *current &= !flags,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        self.parser.process(&frame[offset..]);
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// OSC 52 clipboard writes since the last drain — base64 payloads, still
    /// encoded. Decoding and the actual clipboard belong to the view.
    pub fn take_clipboard_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_writes)
    }

    /// The directory the program last reported through OSC 7, if it has.
    pub fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    /// The kitty keyboard enhancement flags currently in effect — the top of
    /// the flag stack, or zero when nothing was ever pushed.
    fn kitty_flags(&self) -> u8 {
        self.kitty_stack.last().copied().unwrap_or(0)
    }

    /// Kept for when the cursor gets a blink phase — the policy is decided
    /// here rather than at the draw site.
    #[allow(dead_code)]
    fn cursor_blinks(&self) -> bool {
        !self.screen().alternate_screen() || self.cursor_blink
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// The visible grid as styled text runs, one Vec per row. Cells that
    /// share a CellStyle merge into one run so the view emits a handful of
    /// spans per line instead of one per column. The cursor cell is baked in
    /// as a colour-swapped run so the caret always lands exactly on its
    /// glyph — no font-metric guesswork in the view.
    pub fn styled_rows(&self) -> Vec<Vec<StyledRun>> {
        let screen = self.screen();
        let (rows, cols) = screen.size();
        let cursor = if screen.hide_cursor() || screen.scrollback() > 0 {
            None
        } else {
            Some(screen.cursor_position())
        };
        (0..rows)
            .map(|row| {
                let mut runs: Vec<StyledRun> = Vec::new();
                for col in 0..cols {
                    let Some(cell) = screen.cell(row, col) else {
                        continue;
                    };
                    let style = CellStyle::of(cell);
                    let (mut fg, mut bg) = (style.fg, style.bg);
                    let is_cursor = cursor == Some((row, col));
                    if is_cursor {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    let text = cell.contents();
                    if let Some(last) = runs.last_mut() {
                        if !is_cursor
                            && !last.cursor
                            && last.fg == fg
                            && last.bg == bg
                            && last.bold == style.bold
                            && last.italic == style.italic
                            && last.underline == style.underline
                        {
                            last.text.push_str(text);
                            continue;
                        }
                    }
                    runs.push(StyledRun {
                        text: text.to_string(),
                        fg,
                        bg,
                        bold: style.bold || is_cursor,
                        italic: style.italic,
                        underline: style.underline,
                        cursor: is_cursor,
                    });
                }
                runs
            })
            .collect()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser
            .screen_mut()
            .set_size(rows.clamp(1, 300), cols.clamp(1, 500));
    }

    pub fn scroll(&mut self, lines: i32) {
        let offset = self.screen().scrollback();
        let next = if lines >= 0 {
            offset.saturating_add(lines as usize)
        } else {
            offset.saturating_sub(lines.unsigned_abs() as usize)
        };
        self.parser.screen_mut().set_scrollback(next);
    }

    /// Snap to the live edge — xterm's `scrollToBottom` /
    /// `scrollOnUserInput` behavior.
    pub fn scroll_to_bottom(&mut self) {
        self.wheel_remainder = 0.0;
        self.parser.screen_mut().set_scrollback(0);
    }

    /// The viewport's scrollback offset — the coordinate base selections
    /// are anchored against.
    fn scrollback(&self) -> i64 {
        self.screen().scrollback() as i64
    }

    /// Start a selection at a visible-grid cell, replacing any existing one
    /// (xterm clears the old selection on mousedown).
    pub fn selection_begin(&mut self, col: u16, vis_row: u16) {
        let abs = vis_row as i64 - self.scrollback();
        self.selection = Some(Selection {
            anchor: (abs, col),
            head: (abs, col),
        });
    }

    /// Move the selection's head to a visible-grid cell. No-ops without an
    /// active selection.
    pub fn selection_update(&mut self, col: u16, vis_row: u16) {
        let scrollback = self.scrollback();
        if let Some(selection) = &mut self.selection {
            selection.head = (vis_row as i64 - scrollback, col);
        }
    }

    pub fn selection_clear(&mut self) {
        self.selection = None;
    }

    /// The selected cell ranges in *visible* rows, one `(row, start_col,
    /// end_col_exclusive)` per covered row — what the view paints. Rows
    /// scrolled out of the viewport are clipped away; columns clip to the
    /// grid.
    pub fn selection_ranges(&self) -> Vec<(u16, u16, u16)> {
        let Some(selection) = self.selection else {
            return Vec::new();
        };
        let (rows, cols) = self.screen().size();
        let ((top, top_col), (bottom, bottom_col)) = selection.ordered();
        let scrollback = self.scrollback();
        (top..=bottom)
            .filter_map(|abs| {
                let vis = abs + scrollback;
                if vis < 0 || vis >= rows as i64 {
                    return None;
                }
                let (start, end) = if top == bottom {
                    (top_col, bottom_col.saturating_add(1))
                } else if abs == top {
                    (top_col, cols)
                } else if abs == bottom {
                    (0, bottom_col.saturating_add(1))
                } else {
                    (0, cols)
                };
                Some((vis as u16, start.min(cols), end.min(cols)))
            })
            .collect()
    }

    /// The selected text, xterm's `getSelection` shape: one `\n` per covered
    /// grid row, each right-trimmed. Returns `None` for an empty/cleared or
    /// fully offscreen selection.
    pub fn selection_text(&self) -> Option<String> {
        let mut lines = Vec::new();
        let (rows, cols) = self.screen().size();
        for (vis, start, end) in self.selection_ranges() {
            let mut line = String::new();
            for col in start..end {
                if vis >= rows || col >= cols {
                    continue;
                }
                if let Some(cell) = self.screen().cell(vis, col) {
                    line.push_str(cell.contents());
                }
            }
            lines.push(line.trim_end().to_owned());
        }
        let text = lines.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn scroll_pixels(&mut self, pixels: f32, row_height: f32) {
        if self.screen().alternate_screen() {
            self.wheel_remainder = 0.0;
            return;
        }
        if !pixels.is_finite() || !row_height.is_finite() || row_height <= 0.0 {
            return;
        }
        self.wheel_remainder += pixels;
        let lines = (self.wheel_remainder / row_height).trunc() as i32;
        if lines != 0 {
            self.wheel_remainder -= lines as f32 * row_height;
            self.scroll(lines);
        }
    }

    /// Bracketed-paste (DECSET 2004) as the program negotiated it.
    pub fn bracketed_paste(&self) -> bool {
        self.screen().bracketed_paste()
    }

    /// A clipboard/drag-drop payload encoded for the PTY: newlines become
    /// CR, and when the program enabled bracketed paste the payload is
    /// wrapped in `\x1b[200~`/`\x1b[201~` after stripping ESC bytes that
    /// could forge the closing marker early.
    pub fn paste(&mut self, text: &str) -> Vec<u8> {
        self.paste_bytes(text)
    }

    fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let text = text.replace("\r\n", "\n").replace('\n', "\r");
        if self.bracketed_paste() {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
        } else {
            text.into_bytes()
        }
    }

    pub fn input(&self, event: &KeyInput) -> Option<Vec<u8>> {
        match event {
            KeyInput::Text(text) => Some(text.clone().into_bytes()),
            KeyInput::Paste(text) => Some(self.paste_bytes(text)),
            KeyInput::Key { key, modifiers } => {
                self.key_bytes(*key, *modifiers, KeyEventKind::Press)
            }
            KeyInput::KeyEvent {
                key,
                modifiers,
                kind,
            } => self.key_bytes(*key, *modifiers, *kind),
        }
    }

    /// One key event encoded the way the program's negotiated protocols ask
    /// for: the kitty flag stack first, the xterm/VT legacy bytes after.
    fn key_bytes(&self, key: TermKey, modifiers: KeyMods, kind: KeyEventKind) -> Option<Vec<u8>> {
        if modifiers.mac_cmd {
            return None;
        }
        let flags = self.kitty_flags();
        // Without the event-types flag a release produces nothing anywhere
        // and a repeat is just another press — the legacy stream has no way
        // to say either.
        if kind == KeyEventKind::Release && flags & KITTY_EVENT_TYPES == 0 {
            return None;
        }
        let kind = if kind == KeyEventKind::Repeat && flags & KITTY_EVENT_TYPES == 0 {
            KeyEventKind::Press
        } else {
            kind
        };
        // The kitty path drives CSI u whenever the program asked for it:
        // disambiguate (0b1) for ambiguous chords, all-keys (0b1000) for
        // everything, event types (0b10) for releases/repeats of anything.
        let kitty_active = flags & (KITTY_DISAMBIGUATE | KITTY_ALL_KEYS) != 0
            || (kind != KeyEventKind::Press && flags & KITTY_EVENT_TYPES != 0);
        if kitty_active {
            if let Some(bytes) = Self::kitty_encode(key, modifiers, kind, flags) {
                return Some(bytes);
            }
            // A release the CSI-u encoder declined still stays silent —
            // falling through to legacy would re-emit the press bytes.
            if kind == KeyEventKind::Release {
                return None;
            }
        }
        // Only a press or a repeat reaches legacy bytes; a repeat types
        // again, a release is silence wherever it wasn't reported above.
        if kind == KeyEventKind::Release {
            return None;
        }
        if modifiers.ctrl {
            if let Some(byte) = control_byte(key) {
                return Some(meta(modifiers.alt, vec![byte]));
            }
        }
        // Arrows and Home/End take a CSI or SS3 introducer depending
        // on the application cursor mode, and grow a modifier
        // parameter when any modifier is held.
        if let Some(suffix) = cursor_suffix(key) {
            let modifier = 1
                + u8::from(modifiers.shift)
                + 2 * u8::from(modifiers.alt)
                + 4 * u8::from(modifiers.ctrl);
            return Some(
                if modifier > 1 {
                    format!("\x1b[1;{modifier}{suffix}")
                } else if self.screen().application_cursor() {
                    format!("\x1bO{suffix}")
                } else {
                    format!("\x1b[{suffix}")
                }
                .into_bytes(),
            );
        }
        // Insert/Delete/PageUp/PageDown and the F-keys all have xterm
        // modified forms — send them instead of dropping the modifiers.
        if let Some(bytes) = modified_special(key, kitty_modifier(modifiers)) {
            return Some(bytes);
        }
        if let Some(sequence) = function_key(key) {
            return Some(sequence.as_bytes().to_vec());
        }
        if let TermKey::Numpad(c) = key {
            // Application keypad mode (DECNKM) swaps the digits/operators for
            // their SS3 codes; without it the key types its face character.
            if self.screen().application_keypad() {
                if let Some(code) = keypad_ss3(c) {
                    return Some(meta(modifiers.alt, format!("\x1bO{code}").into_bytes()));
                }
            }
            return Some(meta(modifiers.alt, c.to_string().into_bytes()));
        }
        if key == TermKey::NumpadEnter {
            let plain = if self.screen().application_keypad() {
                "\x1bOM"
            } else {
                "\r"
            };
            return Some(meta(modifiers.alt, plain.as_bytes().to_vec()));
        }
        let plain = match key {
            TermKey::Enter => "\r",
            TermKey::Backspace => "\x7f",
            TermKey::Escape => "\x1b",
            TermKey::Tab if modifiers.shift => "\x1b[Z",
            TermKey::Tab => "\t",
            TermKey::Delete => "\x1b[3~",
            TermKey::Insert => "\x1b[2~",
            TermKey::PageUp => "\x1b[5~",
            TermKey::PageDown => "\x1b[6~",
            TermKey::Char(c) => return Some(meta(modifiers.alt, c.to_string().into_bytes())),
            _ => return None,
        };
        Some(meta(modifiers.alt, plain.as_bytes().to_vec()))
    }

    pub fn mouse_reporting(&self) -> bool {
        self.mouse_mode != 0
    }

    /// True while the emulator sits on the alternate buffer — a full-screen
    /// program owns the terminal. The initial-command gate reads this.
    pub fn alternate_screen(&self) -> bool {
        self.screen().alternate_screen()
    }

    /// True while DECSET 1016 is the active mouse encoding — reports then
    /// carry pixel coordinates, so `mouse_event` callers should pass the
    /// pointer's pixel position rather than a cell.
    pub fn mouse_pixels(&self) -> bool {
        self.mouse_encoding == MouseEncoding::SgrPixels
    }

    /// One mouse event at a cell coordinate (0-based; under 1016, a pixel
    /// coordinate instead). What gets reported is the tracking mode's call:
    /// X10 (9) presses only, 1000 adds releases, 1002 adds drags, 1003 adds
    /// all motion. Returns the bytes to write to the PTY, or None when the
    /// program isn't tracking this event.
    pub fn mouse_event(
        &mut self,
        kind: MouseKind,
        col: u16,
        row: u16,
        modifiers: KeyMods,
    ) -> Option<Vec<u8>> {
        if !self.mouse_reporting() {
            return None;
        }
        match kind {
            MouseKind::Press(button) => {
                self.mouse_buttons[button as usize] = true;
                self.mouse_position = Some((col, row));
                self.mouse_report(button as u8, true, col, row, modifiers)
            }
            MouseKind::Release(button) => {
                // X10 reports presses only — and a release for a button we
                // never saw go down isn't an event the program knows about.
                if self.mouse_mode == 9 || !self.mouse_buttons[button as usize] {
                    return None;
                }
                self.mouse_buttons[button as usize] = false;
                self.mouse_position = Some((col, row));
                self.mouse_report(button as u8, false, col, row, modifiers)
            }
            MouseKind::Move(button) => {
                // The caller may not track which button is down; the screen
                // does — a bare Move while one is held is its drag.
                let button = button.or_else(|| {
                    self.mouse_buttons.iter().position(|pressed| *pressed).map(
                        |index| match index {
                            0 => MouseButton::Primary,
                            1 => MouseButton::Middle,
                            _ => MouseButton::Secondary,
                        },
                    )
                });
                // Drag needs 1002+; bare motion needs the any-motion mode.
                let code = match (button, self.mouse_mode) {
                    (Some(button), 1002 | 1003) if self.mouse_buttons[button as usize] => {
                        button as u8 + 32
                    }
                    (None, 1003) => 35,
                    _ => return None,
                };
                if self.mouse_position == Some((col, row)) {
                    return None;
                }
                self.mouse_position = Some((col, row));
                self.mouse_report(code, true, col, row, modifiers)
            }
            // Wheel notches report as button 4/5 (and 6/7 sideways) presses;
            // there is no release half to send.
            MouseKind::WheelUp => self.mouse_report(64, true, col, row, modifiers),
            MouseKind::WheelDown => self.mouse_report(65, true, col, row, modifiers),
            MouseKind::WheelLeft => self.mouse_report(66, true, col, row, modifiers),
            MouseKind::WheelRight => self.mouse_report(67, true, col, row, modifiers),
        }
    }

    /// The same reporting as `mouse_event`, driven from pixel-geometry
    /// events: `body` is the terminal body rect and `cell` the pixel size of
    /// one character cell, both in the caller's coordinate space.
    pub fn mouse_position_event(
        &mut self,
        event: &MouseInput,
        body: (f32, f32, f32, f32),
        cell: (f32, f32),
        modifiers: KeyMods,
    ) -> Option<Vec<u8>> {
        if !self.mouse_reporting() {
            return None;
        }
        let (left, top, right, bottom) = body;
        let coordinates = |(x, y): (f32, f32)| {
            let (rows, cols) = self.screen().size();
            (
                ((x - left) / cell.0)
                    .floor()
                    .clamp(0.0, f32::from(cols.saturating_sub(1))) as u16,
                ((y - top) / cell.1)
                    .floor()
                    .clamp(0.0, f32::from(rows.saturating_sub(1))) as u16,
            )
        };
        let contains = |(x, y): (f32, f32)| x >= left && x < right && y >= top && y < bottom;
        match event {
            MouseInput::Press { button, pos } => {
                let index = *button as usize;
                if !contains(*pos) || self.mouse_buttons[index] {
                    // Press outside the body, or an already-down button:
                    // nothing to report. (Release outside still reports —
                    // the terminal must see the button come up.)
                    if !contains(*pos) {
                        return None;
                    }
                }
                let (col, row) = coordinates(*pos);
                self.mouse_buttons[index] = true;
                self.mouse_position = Some((col, row));
                self.mouse_report(*button as u8, true, col, row, modifiers)
            }
            MouseInput::Release { button, pos } => {
                let index = *button as usize;
                if self.mouse_mode == 9 || !self.mouse_buttons[index] {
                    return None;
                }
                let (col, row) = coordinates(*pos);
                self.mouse_buttons[index] = false;
                self.mouse_position = Some((col, row));
                self.mouse_report(*button as u8, false, col, row, modifiers)
            }
            MouseInput::Move { pos } if self.mouse_mode >= 1002 => {
                let button = self.mouse_buttons.iter().position(|pressed| *pressed);
                if button.is_none() && (self.mouse_mode != 1003 || !contains(*pos)) {
                    return None;
                }
                let (col, row) = coordinates(*pos);
                if self.mouse_position == Some((col, row)) {
                    return None;
                }
                self.mouse_position = Some((col, row));
                self.mouse_report(
                    button.map_or(35, |button| button as u8 + 32),
                    true,
                    col,
                    row,
                    modifiers,
                )
            }
            _ => None,
        }
    }

    /// One mouse report at the given cell. `button` is the xterm code: 0/1/2
    /// for the three buttons, 32+ for motion, 64-67 for the wheel.
    pub fn mouse_report(
        &self,
        button: u8,
        pressed: bool,
        column: u16,
        row: u16,
        modifiers: KeyMods,
    ) -> Option<Vec<u8>> {
        if !self.mouse_reporting() {
            return None;
        }
        let code = button
            + 4 * u8::from(modifiers.shift)
            + 8 * u8::from(modifiers.alt)
            + 16 * u8::from(modifiers.ctrl);
        // Coordinates are 1-based in every encoding.
        let (column, row) = (column.saturating_add(1), row.saturating_add(1));
        match self.mouse_encoding {
            MouseEncoding::Sgr | MouseEncoding::SgrPixels => {
                // The final byte marks press vs release, so the button keeps
                // its identity on the way up.
                let final_byte = if pressed { 'M' } else { 'm' };
                Some(format!("\x1b[<{code};{column};{row}{final_byte}").into_bytes())
            }
            MouseEncoding::Urxvt => {
                // Decimal fields biased by 32; releases degrade to the
                // anonymous button 3 just like the byte encoding.
                let code = if pressed {
                    code
                } else {
                    3 + 4 * u8::from(modifiers.shift)
                        + 8 * u8::from(modifiers.alt)
                        + 16 * u8::from(modifiers.ctrl)
                };
                Some(format!("\x1b[{};{column};{row}M", code + 32).into_bytes())
            }
            MouseEncoding::Default | MouseEncoding::Utf8 => {
                // One byte per field and no room to say which button came
                // up — every release is reported as button 3.
                let code = if pressed {
                    code
                } else {
                    3 + 4 * u8::from(modifiers.shift)
                        + 8 * u8::from(modifiers.alt)
                        + 16 * u8::from(modifiers.ctrl)
                };
                if self.mouse_encoding == MouseEncoding::Default {
                    let clamp = |value: u16| u8::try_from(value.min(223)).unwrap_or(223) + 32;
                    Some(vec![0x1b, b'[', b'M', code + 32, clamp(column), clamp(row)])
                } else {
                    // DECSET 1005: the same three fields, each UTF-8 encoded
                    // so coordinates past 223 stay representable.
                    let field = |value: u16| {
                        char::from_u32(u32::from(value) + 32)
                            .map(|c| c.to_string())
                            .unwrap_or_default()
                    };
                    Some(
                        format!(
                            "\x1b[M{}{}{}",
                            field(u16::from(code)),
                            field(column),
                            field(row)
                        )
                        .into_bytes(),
                    )
                }
            }
        }
    }

    /// The report a terminal expects when the window gains or loses focus,
    /// for programs that asked for it with DECSET 1004: `\x1b[I` on focus,
    /// `\x1b[O` on unfocus, `None` when focus reporting is off.
    pub fn focus_event(&mut self, gained: bool) -> Option<Vec<u8>> {
        self.focus_reporting.then(|| {
            if gained {
                b"\x1b[I".to_vec()
            } else {
                b"\x1b[O".to_vec()
            }
        })
    }

    /// The DECRPM answer for one private mode: 1 set, 2 reset, 0 when it is
    /// a mode this screen doesn't track at all.
    fn mode_state(&self, mode: u16) -> u8 {
        let set = match mode {
            1 => self.screen().application_cursor(),
            9 | 1000 | 1002 | 1003 => self.mouse_mode == mode,
            12 => self.cursor_blink,
            25 => !self.screen().hide_cursor(),
            1004 => self.focus_reporting,
            1005 => self.mouse_encoding == MouseEncoding::Utf8,
            1006 => self.mouse_encoding == MouseEncoding::Sgr,
            1015 => self.mouse_encoding == MouseEncoding::Urxvt,
            1016 => self.mouse_encoding == MouseEncoding::SgrPixels,
            47 | 1047 | 1049 => self.screen().alternate_screen(),
            2004 => self.bracketed_paste(),
            2026 => self.sync_started.is_some(),
            _ => return 0,
        };
        if set {
            1
        } else {
            2
        }
    }

    /// XTGETTCAP: `DCS + q <hex names> ST` asks for terminfo values. Known
    /// names go out as `DCS 1 + r <hex name>=<hex value>`, unknown as
    /// `DCS 0 + r <hex name>` — both lists keeping the query's own hex.
    fn tcap_reply(&mut self, payload: &[u8]) {
        const CAPS: &[(&[u8], &[u8])] = &[
            (b"TN", b"xterm-256color"),
            (b"name", b"xterm-256color"),
            (b"Co", b"256"),
            (b"colors", b"256"),
            (b"RGB", b"8/8/8"),
            (b"Tc", b"true"),
        ];
        let mut known: Vec<u8> = Vec::new();
        let mut unknown: Vec<u8> = Vec::new();
        for segment in payload.split(|b| *b == b';').filter(|s| !s.is_empty()) {
            let name = hex_decode(segment);
            let value = name
                .as_deref()
                .and_then(|name| CAPS.iter().find(|(cap, _)| *cap == name).map(|(_, v)| *v));
            match value {
                Some(value) => {
                    if !known.is_empty() {
                        known.push(b';');
                    }
                    known.extend_from_slice(segment);
                    known.push(b'=');
                    known.extend_from_slice(hex_encode(value).as_bytes());
                }
                None => {
                    if !unknown.is_empty() {
                        unknown.push(b';');
                    }
                    unknown.extend_from_slice(segment);
                }
            }
        }
        if !known.is_empty() {
            self.replies.extend_from_slice(b"\x1bP1+r");
            self.replies.extend_from_slice(&known);
            self.replies.extend_from_slice(b"\x1b\\");
        }
        if !unknown.is_empty() {
            self.replies.extend_from_slice(b"\x1bP0+r");
            self.replies.extend_from_slice(&unknown);
            self.replies.extend_from_slice(b"\x1b\\");
        }
    }
}

/// Alt is Meta: the same bytes, prefixed with ESC.
fn meta(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// The C0 byte a Ctrl chord sends. Ctrl+letter is the letter's position in
/// the alphabet; the punctuation chords are the rest of the C0 range, and
/// programs read Ctrl+Space as NUL.
fn control_byte(key: TermKey) -> Option<u8> {
    // Ctrl+letter is the letter's position in the alphabet; Ctrl+Space is NUL.
    if let TermKey::Char(c) = key {
        let byte = (c as u8).to_ascii_uppercase();
        if byte.is_ascii_uppercase() {
            return Some(byte - b'A' + 1);
        }
        // The punctuation chords share the letter rule's encoding: each
        // punctuation alias maps to the C0 byte xterm gives it.
        return Some(match c {
            '@' | '2' => 0x00,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '/' | '7' | '-' | '?' => 0x1f,
            ' ' => 0x00,
            '8' => 0x7f,
            _ => return None,
        });
    }
    Some(match key {
        TermKey::Space => 0x00,
        TermKey::Backspace => 0x08,
        _ => return None,
    })
}

/// Kitty/xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn kitty_modifier(modifiers: KeyMods) -> u8 {
    1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl)
}

/// The kitty functional-key codepoints beyond the plain Unicode ones —
/// arrows, navigation, F-keys and the keypad live above 57344 in the spec.
fn kitty_function_codepoint(key: TermKey) -> Option<u32> {
    Some(match key {
        TermKey::F1 => 57364,
        TermKey::F2 => 57365,
        TermKey::F3 => 57366,
        TermKey::F4 => 57367,
        TermKey::F5 => 57368,
        TermKey::F6 => 57369,
        TermKey::F7 => 57370,
        TermKey::F8 => 57371,
        TermKey::F9 => 57372,
        TermKey::F10 => 57373,
        TermKey::F11 => 57374,
        TermKey::F12 => 57375,
        TermKey::Numpad(c) => match c {
            '0'..='9' => 57399 + u32::from(c as u8 - b'0'),
            '.' => 57409,
            '/' => 57410,
            '*' => 57411,
            '-' => 57412,
            '+' => 57413,
            '=' => 57415,
            ',' => 57416,
            _ => return None,
        },
        TermKey::NumpadEnter => 57414,
        TermKey::ArrowLeft => 57417,
        TermKey::ArrowRight => 57418,
        TermKey::ArrowUp => 57419,
        TermKey::ArrowDown => 57420,
        TermKey::PageUp => 57421,
        TermKey::PageDown => 57422,
        TermKey::Home => 57423,
        TermKey::End => 57424,
        TermKey::Insert => 57425,
        TermKey::Delete => 57426,
        _ => return None,
    })
}

/// The SS3 a keypad key sends under application keypad mode (DECNKM) —
/// digits map to `\x1bOp` … `\x1bOy`, operators to their own letters.
fn keypad_ss3(c: char) -> Option<char> {
    Some(match c {
        '0'..='9' => (b'p' + (c as u8 - b'0')) as char,
        '.' => 'n',
        ',' => 'l',
        '-' => 'm',
        '+' => 'k',
        '*' => 'j',
        '/' => 'o',
        _ => return None,
    })
}

impl TerminalScreen {
    /// The CSI-u encoding of one key event under the current kitty flags, or
    /// None when the legacy bytes carry it unambiguously. Flags compose:
    /// 0b1 disambiguates modified chords, 0b10 adds the `:event` subfield to
    /// every reported event, 0b100 appends `:shifted` alternates, 0b1000
    /// reports every key — and 0b10000 appends the associated text.
    fn kitty_encode(
        key: TermKey,
        modifiers: KeyMods,
        kind: KeyEventKind,
        flags: u8,
    ) -> Option<Vec<u8>> {
        let all = flags & KITTY_ALL_KEYS != 0;
        let event = (flags & KITTY_EVENT_TYPES != 0).then_some(match kind {
            KeyEventKind::Press => 1u8,
            KeyEventKind::Repeat => 2,
            KeyEventKind::Release => 3,
        });
        let bare = modifiers == KeyMods::default();
        // Enter/Tab/Backspace unmodified keep their legacy bytes even under
        // event types — only report-all turns them into CSI u (herdr parity).
        if !all && bare && matches!(key, TermKey::Enter | TermKey::Tab | TermKey::Backspace) {
            return None;
        }
        if bare && event.is_none() && !all {
            return None;
        }
        // Special keys keep their xterm modified form unless the program
        // asked for event types or every-key reporting — even Ghostty sends
        // arrows as `1;{mod}A` with disambiguate on.
        if event.is_none()
            && !all
            && (cursor_suffix(key).is_some()
                || matches!(
                    key,
                    TermKey::Insert
                        | TermKey::Delete
                        | TermKey::PageUp
                        | TermKey::PageDown
                        | TermKey::F1
                        | TermKey::F2
                        | TermKey::F3
                        | TermKey::F4
                        | TermKey::F5
                        | TermKey::F6
                        | TermKey::F7
                        | TermKey::F8
                        | TermKey::F9
                        | TermKey::F10
                        | TermKey::F11
                        | TermKey::F12
                ))
        {
            return None;
        }
        // A plain or Shift-only character is text, not an escape — CSI u is
        // for chords legacy can't say (ctrl/alt), every key under 0b1000,
        // and releases, which have no legacy form at all.
        if !all
            && kind != KeyEventKind::Release
            && !modifiers.alt
            && !modifiers.ctrl
            && matches!(key, TermKey::Char(_) | TermKey::Space)
        {
            return None;
        }
        let (codepoint, alternate) = match key {
            // A character chord reports the key's codepoint; with Shift held
            // the unshifted (lowercase) key is what the program expects back.
            TermKey::Char(c) => {
                let (base, shifted) = if modifiers.shift && c.is_ascii_uppercase() {
                    (c.to_ascii_lowercase() as u32, Some(c as u32))
                } else {
                    (c as u32, None)
                };
                let alternate = (flags & KITTY_ALTERNATE_KEYS != 0)
                    .then_some(shifted)
                    .flatten();
                (base, alternate)
            }
            TermKey::Space => (32, None),
            TermKey::Enter => (13, None),
            TermKey::Tab => (9, None),
            TermKey::Backspace => (127, None),
            TermKey::Escape => (27, None),
            other => (kitty_function_codepoint(other)?, None),
        };
        let modifier = kitty_modifier(modifiers);
        let mut sequence = format!("\x1b[{codepoint}");
        if let Some(shifted) = alternate {
            sequence.push_str(&format!(":{shifted}"));
        }
        sequence.push_str(&format!(";{modifier}"));
        if let Some(event) = event {
            sequence.push_str(&format!(":{event}"));
        }
        // Associated text (0b10000): the codepoint the press produced, for
        // keys whose modifier didn't consume the text — none on releases.
        if flags & KITTY_ASSOCIATED_TEXT != 0 && kind == KeyEventKind::Press {
            let text = match key {
                TermKey::Char(c)
                    if (modifiers == KeyMods::default()
                        || modifiers
                            == KeyMods {
                                shift: true,
                                ..KeyMods::default()
                            })
                        && !c.is_control() =>
                {
                    Some(c as u32)
                }
                TermKey::Space if bare => Some(32),
                _ => None,
            };
            if let Some(text) = text {
                sequence.push_str(&format!(";{text}"));
            }
        }
        sequence.push('u');
        Some(sequence.into_bytes())
    }
}

/// The xterm modified form for special keys — `\x1b[2;5~` for Ctrl+Insert,
/// `\x1b[1;6P` for Ctrl+Shift+F1. Kitty mode still uses these: CSI u is for
/// keys legacy cannot express, and these already can.
fn modified_special(key: TermKey, modifier: u8) -> Option<Vec<u8>> {
    if modifier <= 1 {
        return None;
    }
    let sequence = match key {
        TermKey::Insert => format!("\x1b[2;{modifier}~"),
        TermKey::Delete => format!("\x1b[3;{modifier}~"),
        TermKey::PageUp => format!("\x1b[5;{modifier}~"),
        TermKey::PageDown => format!("\x1b[6;{modifier}~"),
        TermKey::F1 => format!("\x1b[1;{modifier}P"),
        TermKey::F2 => format!("\x1b[1;{modifier}Q"),
        TermKey::F3 => format!("\x1b[1;{modifier}R"),
        TermKey::F4 => format!("\x1b[1;{modifier}S"),
        TermKey::F5 => format!("\x1b[15;{modifier}~"),
        TermKey::F6 => format!("\x1b[17;{modifier}~"),
        TermKey::F7 => format!("\x1b[18;{modifier}~"),
        TermKey::F8 => format!("\x1b[19;{modifier}~"),
        TermKey::F9 => format!("\x1b[20;{modifier}~"),
        TermKey::F10 => format!("\x1b[21;{modifier}~"),
        TermKey::F11 => format!("\x1b[23;{modifier}~"),
        TermKey::F12 => format!("\x1b[24;{modifier}~"),
        _ => return None,
    };
    Some(sequence.into_bytes())
}

fn cursor_suffix(key: TermKey) -> Option<&'static str> {
    Some(match key {
        TermKey::ArrowUp => "A",
        TermKey::ArrowDown => "B",
        TermKey::ArrowRight => "C",
        TermKey::ArrowLeft => "D",
        TermKey::Home => "H",
        TermKey::End => "F",
        _ => return None,
    })
}

/// xterm's function keys: F1–F4 are SS3, the rest are CSI with a number.
fn function_key(key: TermKey) -> Option<&'static str> {
    Some(match key {
        TermKey::F1 => "\x1bOP",
        TermKey::F2 => "\x1bOQ",
        TermKey::F3 => "\x1bOR",
        TermKey::F4 => "\x1bOS",
        TermKey::F5 => "\x1b[15~",
        TermKey::F6 => "\x1b[17~",
        TermKey::F7 => "\x1b[18~",
        TermKey::F8 => "\x1b[19~",
        TermKey::F9 => "\x1b[20~",
        TermKey::F10 => "\x1b[21~",
        TermKey::F11 => "\x1b[23~",
        TermKey::F12 => "\x1b[24~",
        _ => return None,
    })
}

/// A run of cells sharing one style — the unit the view hands to the text
/// renderer. `bg` is the resolved cell background so inverse/video-reversed
/// text still paints its block.
#[derive(Clone, Debug)]
pub struct StyledRun {
    pub text: String,
    pub fg: CellRgb,
    pub bg: CellRgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// True for the single run that is the cursor cell; its colours are
    /// already swapped, this flag only exists so a neighbour never merges
    /// into it.
    pub cursor: bool,
}

/// Everything about a cell that decides how its glyph is drawn. Cells that
/// agree on all of it can share one text call.
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: CellRgb,
    bg: CellRgb,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell) -> Self {
        let mut fg = color(cell.fgcolor(), (232, 232, 234));
        let mut bg = color(cell.bgcolor(), crate::theme::monochrome::BASE);
        if cell.inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }
        // SGR 2 is "faint": the same colour at lower intensity. Agent TUIs
        // lean on it for every secondary line, so ignoring it flattened their
        // whole interface into one weight.
        if cell.dim() {
            fg = (
                (fg.0 as f32 * 0.55) as u8,
                (fg.1 as f32 * 0.55) as u8,
                (fg.2 as f32 * 0.55) as u8,
            );
        }
        Self {
            fg,
            bg,
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
        }
    }
}

/// XTGETTCAP hexifies both names and values; the query arrives hex-encoded
/// and the answer echoes names in the same alphabet.
fn hex_decode(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let pair = |pair: &[u8; 2]| {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        Some((hi << 4 | lo) as u8)
    };
    text.as_chunks::<2>().0.iter().map(pair).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn color(value: vt100::Color, default: CellRgb) -> CellRgb {
    const ANSI: [u32; 16] = [
        0x050506, 0xf07178, 0x7fd99a, 0xe6c07b, 0x7aa2f7, 0xc792ea, 0x7dcfff, 0xd4d4d8, 0x6b6b74,
        0xff8b92, 0x95e6a8, 0xf0d48a, 0x9ab8ff, 0xd7a6f5, 0x9de8ff, 0xffffff,
    ];
    match value {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => (r, g, b),
        vt100::Color::Idx(index @ 0..=15) => {
            let rgb = ANSI[index as usize];
            ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
        }
        vt100::Color::Idx(index @ 16..=231) => {
            let index = index - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
            (level(index / 36), level(index / 6 % 6), level(index % 6))
        }
        vt100::Color::Idx(index) => {
            let g = 8 + (index - 232) * 10;
            (g, g, g)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orange_truecolor_survives_every_transport_split() {
        let bytes = b"\x1b[38;2;217;119;87mClaude\x1b[0m";
        for split in 0..=bytes.len() {
            let mut screen = super::TerminalScreen::default();
            screen.process(&bytes[..split]);
            screen.process(&bytes[split..]);
            assert_eq!(
                super::CellStyle::of(screen.screen().cell(0, 0).unwrap()).fg,
                (217, 119, 87)
            );
        }
    }

    #[test]
    fn utf8_and_escape_sequences_survive_split_writes() {
        let mut terminal = TerminalScreen::default();
        for byte in "\x1b[31m\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{754c}".as_bytes() {
            terminal.process(&[*byte]);
        }
        assert_eq!(
            terminal.screen().contents(),
            "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{754c}"
        );
        assert_eq!(
            terminal.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        assert!(terminal.screen().cell(0, 7).unwrap().is_wide());
    }

    #[test]
    fn cursor_updates_replace_text_and_alternate_screen_restores_it() {
        let mut terminal = TerminalScreen::default();
        terminal.process(b"old\rnew\x1b[?1049hother\x1b[?1049l");
        assert_eq!(terminal.screen().contents(), "new");
    }

    /// A program that turns blinking off must not be given a blinking caret:
    /// an agent that draws its own is the case this broke.
    #[test]
    fn the_program_owns_cursor_blinking() {
        let mut terminal = TerminalScreen::default();
        assert!(terminal.cursor_blink);
        terminal.process(b"\x1b[?12l");
        assert!(!terminal.cursor_blink);
        terminal.process(b"\x1b[?12h");
        assert!(terminal.cursor_blink);
        // DECSCUSR: an even parameter is the steady variant of its shape.
        terminal.process(b"\x1b[4 q");
        assert!(!terminal.cursor_blink);
        // Style 0 restores the default blinking cursor.
        terminal.process(b"\x1b[?12h\x1b[0 q");
        assert!(terminal.cursor_blink);
    }

    #[test]
    fn cursor_commands_keep_stream_order_at_every_chunk_boundary() {
        let stream = b"\x1b[?1049h\x1b[5 q\x1b[?12l";
        for chunk_size in 1..=stream.len() {
            let mut terminal = TerminalScreen::default();
            for chunk in stream.chunks(chunk_size) {
                terminal.process(chunk);
            }
            assert!(!terminal.cursor_blinks(), "chunk size {chunk_size}");
            terminal.process(b"x");
            assert!(!terminal.cursor_blinks());
            terminal.process(b"\x1b[?1049l");
            assert!(terminal.cursor_blinks());
        }
    }

    #[test]
    fn sync_frames_do_not_expose_cursor_changes_early() {
        let mut terminal = TerminalScreen::default();
        terminal.process(b"\x1b[?1049h\x1b[?2026h\x1b[6 qhalf");
        assert_eq!(terminal.screen().contents(), "");
        terminal.process(b" done\x1b[?2026l\x1b[?2026hnext");
        assert_eq!(terminal.screen().contents(), "half done");
        assert!(!terminal.cursor_blinks());
        terminal.sync_started = Some(Instant::now() - MAX_SYNC_TIME);
        terminal.flush_expired();
        assert_eq!(terminal.screen().contents(), "half donenext");
    }

    #[test]
    fn queries_report_the_cursor_at_the_query_and_reset_clears_modes() {
        let mut terminal = TerminalScreen::default();
        terminal.process(b"abc\x1b[6nxyz\x1b[5n\x1b[c");
        assert_eq!(terminal.take_replies(), b"\x1b[1;4R\x1b[0n\x1b[?1;2c");
        terminal.process(b"\x1b[?1004;1006;1000h\x1b[6 q\x1bc");
        assert_eq!(terminal.focus_event(true), None);
        assert!(!terminal.mouse_reporting());
    }

    #[test]
    fn a_synchronized_frame_is_shown_whole_or_not_at_all() {
        let mut terminal = TerminalScreen::default();
        terminal.process(b"\x1b[?2026hhalf");
        assert_eq!(
            terminal.screen().contents(),
            "",
            "a half-drawn frame reached the screen"
        );
        terminal.process(b" a frame\x1b[?2026l");
        assert_eq!(terminal.screen().contents(), "half a frame");
    }

    #[test]
    fn modes_split_across_two_reads_are_still_seen() {
        let mut terminal = TerminalScreen::default();
        terminal.process(b"\x1b[?10");
        terminal.process(b"04h");
        assert_eq!(terminal.focus_event(true), Some(b"\x1b[I".to_vec()));
    }

    #[test]
    fn mouse_reports_only_go_out_once_a_program_asks_for_them() {
        let mut terminal = TerminalScreen::default();
        let modifiers = KeyMods::default();
        assert_eq!(terminal.mouse_report(0, true, 3, 4, modifiers), None);
        terminal.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            terminal.mouse_report(0, true, 3, 4, modifiers),
            Some(b"\x1b[<0;4;5M".to_vec())
        );
        assert_eq!(
            terminal.mouse_report(0, false, 3, 4, modifiers),
            Some(b"\x1b[<0;4;5m".to_vec())
        );
    }

    #[test]
    fn function_keys_and_meta_reach_the_shell() {
        let terminal = TerminalScreen::default();
        let key = |key, modifiers| terminal.input(&KeyInput::Key { key, modifiers });
        let none = KeyMods::default();
        assert_eq!(key(TermKey::F5, none), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key(TermKey::F1, none), Some(b"\x1bOP".to_vec()));
        assert_eq!(
            key(TermKey::Char('c'), KeyMods { ctrl: true, ..none }),
            Some(vec![3])
        );
        assert_eq!(
            key(TermKey::Space, KeyMods { ctrl: true, ..none }),
            Some(vec![0])
        );
        assert_eq!(
            key(TermKey::Enter, KeyMods { alt: true, ..none }),
            Some(b"\x1b\r".to_vec())
        );
    }

    #[test]
    fn pointer_clicks_dragging_and_release_outside_the_card_reach_the_terminal() {
        let mut screen = TerminalScreen::default();
        screen.process(b"\x1b[?1002h\x1b[?1006h");
        let body = (0.0, 0.0, 120.0, 32.0);
        let cell = (1.0, 1.0);
        let mods = KeyMods::default();
        let click = |button: MouseButton, pressed: bool, pos: (f32, f32)| {
            if pressed {
                MouseInput::Press { button, pos }
            } else {
                MouseInput::Release { button, pos }
            }
        };
        for (button, code) in [
            (MouseButton::Primary, 0),
            (MouseButton::Middle, 1),
            (MouseButton::Secondary, 2),
        ] {
            assert_eq!(
                screen.mouse_position_event(&click(button, true, (3.0, 4.0)), body, cell, mods),
                Some(format!("\x1b[<{code};4;5M").into_bytes())
            );
            assert_eq!(
                screen.mouse_position_event(&click(button, false, (3.0, 4.0)), body, cell, mods),
                Some(format!("\x1b[<{code};4;5m").into_bytes())
            );
        }
        assert!(screen
            .mouse_position_event(&MouseInput::Move { pos: (5.0, 5.0) }, body, cell, mods)
            .is_none());
        screen.mouse_position_event(
            &click(MouseButton::Primary, true, (3.0, 4.0)),
            body,
            cell,
            mods,
        );
        assert_eq!(
            screen.mouse_position_event(&MouseInput::Move { pos: (5.0, 5.0) }, body, cell, mods),
            Some(b"\x1b[<32;6;6M".to_vec())
        );
        assert_eq!(
            screen.mouse_position_event(
                &click(MouseButton::Primary, false, (200.0, 40.0)),
                body,
                cell,
                mods
            ),
            Some(b"\x1b[<0;120;32m".to_vec())
        );
        screen.process(b"\x1b[?1003h");
        assert_eq!(
            screen.mouse_position_event(&MouseInput::Move { pos: (5.0, 5.0) }, body, cell, mods),
            Some(b"\x1b[<35;6;6M".to_vec())
        );
        screen.process(b"\x1bc");
        assert!(screen
            .mouse_position_event(
                &click(MouseButton::Primary, true, (0.0, 0.0)),
                body,
                cell,
                mods
            )
            .is_none());
    }

    #[test]
    fn dimensions_are_bounded() {
        let mut terminal = TerminalScreen::default();
        terminal.resize(0, 0);
        assert_eq!(terminal.screen().size(), (1, 1));
        terminal.resize(u16::MAX, u16::MAX);
        assert_eq!(terminal.screen().size(), (300, 500));
    }

    #[test]
    fn paste_uses_terminal_mode_and_cannot_end_its_own_bracket() {
        let mut terminal = TerminalScreen::default();
        assert_eq!(
            terminal.input(&KeyInput::Paste("a\r\nb".into())).unwrap(),
            b"a\rb"
        );
        terminal.process(b"\x1b[?2004h");
        assert_eq!(
            terminal
                .input(&KeyInput::Paste("a\x1b[201~b".into()))
                .unwrap(),
            b"\x1b[200~a[201~b\x1b[201~"
        );
    }

    #[test]
    fn scrollback_is_bounded_and_returns_to_live_output() {
        let mut terminal = TerminalScreen::default();
        terminal.resize(2, 20);
        terminal.process(b"one\r\ntwo\r\nthree");
        terminal.scroll(1);
        assert_eq!(terminal.screen().contents(), "one\ntwo");
        terminal.scroll(i32::MAX);
        assert_eq!(terminal.screen().scrollback(), 1);
        terminal.scroll(i32::MIN);
        assert_eq!(terminal.screen().contents(), "two\nthree");
    }

    #[test]
    fn trackpad_accumulates_fractional_rows() {
        let mut terminal = TerminalScreen::default();
        terminal.resize(2, 20);
        terminal.process(b"one\r\ntwo\r\nthree");
        for _ in 0..3 {
            terminal.scroll_pixels(4.0, 16.0);
        }
        assert_eq!(terminal.screen().scrollback(), 0);
        terminal.scroll_pixels(4.0, 16.0);
        assert_eq!(terminal.screen().scrollback(), 1);
    }

    #[test]
    fn arrow_keys_follow_application_mode_and_control_keys_are_bytes() {
        let key = |key, modifiers| KeyInput::Key { key, modifiers };
        let mut terminal = TerminalScreen::default();
        assert_eq!(
            terminal
                .input(&key(TermKey::ArrowUp, KeyMods::default()))
                .unwrap(),
            b"\x1b[A"
        );
        terminal.process(b"\x1b[?1h");
        assert_eq!(
            terminal
                .input(&key(TermKey::ArrowUp, KeyMods::default()))
                .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            terminal
                .input(&key(
                    TermKey::Char('c'),
                    KeyMods {
                        ctrl: true,
                        ..KeyMods::default()
                    }
                ))
                .unwrap(),
            [3]
        );
        assert_eq!(
            terminal
                .input(&key(
                    TermKey::ArrowUp,
                    KeyMods {
                        shift: true,
                        ..KeyMods::default()
                    }
                ))
                .unwrap(),
            b"\x1b[1;2A"
        );
    }

    #[test]
    fn osc52_writes_queue_and_osc7_reports_cwd() {
        let mut terminal = TerminalScreen::default();
        terminal.process(b"\x1b]52;c;aGVsbG8=\x07split\x1b]52;p;b3RoZXI=\x1b\\");
        terminal.process(b"\x1b]7;file://host/home/user/code%20base\x07");
        assert_eq!(
            terminal.take_clipboard_writes(),
            vec!["aGVsbG8=".to_owned(), "b3RoZXI=".to_owned()]
        );
        assert_eq!(terminal.cwd().as_deref(), Some("/home/user/code base"));
        assert!(terminal.take_clipboard_writes().is_empty());
    }

    #[test]
    fn kitty_flag_stack_pushes_pops_and_queries() {
        let mut terminal = TerminalScreen::default();
        assert_eq!(terminal.kitty_flags(), 0);
        terminal.process(b"\x1b[?u");
        assert_eq!(terminal.take_replies(), b"\x1b[?0u");
        terminal.process(b"\x1b[>1u\x1b[>5u");
        assert_eq!(terminal.kitty_flags(), 5);
        terminal.process(b"\x1b[?u");
        assert_eq!(terminal.take_replies(), b"\x1b[?5u");
        // Set on the stack top: mode 2 sets bits, mode 3 clears, mode 1 assigns.
        terminal.process(b"\x1b[=2;2u\x1b[=4;3u\x1b[=7u");
        assert_eq!(terminal.kitty_flags(), 7);
        terminal.process(b"\x1b[<u");
        assert_eq!(terminal.kitty_flags(), 1);
        // Popping past the bottom resets everything rather than underflowing.
        terminal.process(b"\x1b[<9u");
        assert_eq!(terminal.kitty_flags(), 0);
        terminal.process(b"\x1bc\x1b[>3u\x1bc");
        assert_eq!(terminal.kitty_flags(), 0);
    }

    #[test]
    fn kitty_disambiguate_sends_csi_u_for_ambiguous_chords_only() {
        let mut terminal = TerminalScreen::default();
        let key = |terminal: &TerminalScreen, key, modifiers| {
            terminal.input(&KeyInput::Key { key, modifiers })
        };
        let none = KeyMods::default();
        let shift = KeyMods {
            shift: true,
            ..none
        };
        let alt = KeyMods { alt: true, ..none };
        let ctrl = KeyMods { ctrl: true, ..none };

        // flags=0: the legacy path is byte-identical.
        assert_eq!(key(&terminal, TermKey::Enter, shift), Some(b"\r".to_vec()));
        assert_eq!(
            key(&terminal, TermKey::Tab, shift),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(key(&terminal, TermKey::Char('c'), ctrl), Some(vec![3]));

        terminal.process(b"\x1b[>1u");
        assert_eq!(
            key(&terminal, TermKey::Enter, shift),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Enter, alt),
            Some(b"\x1b[13;3u".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Tab, shift),
            Some(b"\x1b[9;2u".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Backspace, alt),
            Some(b"\x1b[127;3u".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Char('c'), ctrl),
            Some(b"\x1b[99;5u".to_vec())
        );
        assert_eq!(
            key(
                &terminal,
                TermKey::Char('L'),
                KeyMods {
                    ctrl: true,
                    shift: true,
                    ..none
                }
            ),
            Some(b"\x1b[108;6u".to_vec())
        );
        // Keys that stay legacy: unmodified, or Shift alone producing text.
        assert_eq!(key(&terminal, TermKey::Enter, none), Some(b"\r".to_vec()));
        assert_eq!(
            key(&terminal, TermKey::Char('L'), shift),
            Some(b"L".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Char('a'), none),
            Some(b"a".to_vec())
        );
        // Special keys keep their xterm modified form even with kitty on.
        assert_eq!(
            key(&terminal, TermKey::ArrowUp, shift),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Delete, alt),
            Some(b"\x1b[3;3~".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::F5, shift),
            Some(b"\x1b[15;2~".to_vec())
        );

        // Popping restores the legacy encodings.
        terminal.process(b"\x1b[<u");
        assert_eq!(key(&terminal, TermKey::Enter, shift), Some(b"\r".to_vec()));
        assert_eq!(key(&terminal, TermKey::Char('c'), ctrl), Some(vec![3]));
    }

    #[test]
    fn mouse_event_reports_each_tracking_modes_events_in_each_encoding() {
        let mut screen = TerminalScreen::default();
        let mods = KeyMods::default();
        // Nothing is reported before a program asks, whatever the encoding.
        screen.process(b"\x1b[?1006h");
        assert!(screen
            .mouse_event(MouseKind::Press(MouseButton::Primary), 3, 4, mods)
            .is_none());
        // X10 (mode 9) sends presses only: no release, no motion.
        screen.process(b"\x1b[?9h\x1b[?1000l");
        assert_eq!(
            screen.mouse_event(MouseKind::Press(MouseButton::Primary), 3, 4, mods),
            Some(b"\x1b[<0;4;5M".to_vec())
        );
        assert!(screen
            .mouse_event(MouseKind::Release(MouseButton::Primary), 3, 4, mods)
            .is_none());
        assert!(screen
            .mouse_event(MouseKind::Move(Some(MouseButton::Primary)), 5, 4, mods)
            .is_none());
        // 1000 adds releases; 1003 reports bare motion and the wheel too.
        screen.process(b"\x1b[?9l\x1b[?1003h");
        screen.mouse_event(MouseKind::Press(MouseButton::Primary), 3, 4, mods);
        assert_eq!(
            screen.mouse_event(MouseKind::Release(MouseButton::Primary), 3, 4, mods),
            Some(b"\x1b[<0;4;5m".to_vec())
        );
        assert_eq!(
            screen.mouse_event(MouseKind::WheelUp, 3, 4, mods),
            Some(b"\x1b[<64;4;5M".to_vec())
        );
        assert_eq!(
            screen.mouse_event(MouseKind::WheelDown, 3, 4, mods),
            Some(b"\x1b[<65;4;5M".to_vec())
        );
        // Same cell twice is one report; a drag keeps its button.
        assert!(screen
            .mouse_event(MouseKind::Move(None), 3, 4, mods)
            .is_none());
        assert_eq!(
            screen.mouse_event(MouseKind::Move(None), 6, 7, mods),
            Some(b"\x1b[<35;7;8M".to_vec())
        );
        // Modifiers ride along in the button code.
        assert_eq!(
            screen.mouse_event(
                MouseKind::Press(MouseButton::Secondary),
                1,
                1,
                KeyMods {
                    ctrl: true,
                    shift: true,
                    ..KeyMods::default()
                }
            ),
            Some(b"\x1b[<22;2;2M".to_vec())
        );
    }

    #[test]
    fn mouse_encodings_switch_with_the_last_decset() {
        let mut screen = TerminalScreen::default();
        let mods = KeyMods::default();
        screen.process(b"\x1b[?1000h");
        assert_eq!(
            screen.mouse_event(MouseKind::Press(MouseButton::Middle), 0, 0, mods),
            Some(vec![0x1b, b'[', b'M', 1 + 32, 1 + 32, 1 + 32])
        );
        // urxvt 1015: decimal fields biased by 32, release loses its button.
        screen.process(b"\x1b[?1015h");
        assert_eq!(
            screen.mouse_event(MouseKind::Press(MouseButton::Middle), 0, 0, mods),
            Some(b"\x1b[33;1;1M".to_vec())
        );
        assert_eq!(
            screen.mouse_event(MouseKind::Release(MouseButton::Middle), 0, 0, mods),
            Some(b"\x1b[35;1;1M".to_vec())
        );
        // UTF-8 1005 keeps the one-byte-per-field layout — and wins the
        // shared encoding slot over the earlier 1015.
        screen.process(b"\x1b[?1005h");
        assert_eq!(
            screen.mouse_event(MouseKind::Press(MouseButton::Middle), 0, 0, mods),
            Some(vec![0x1b, b'[', b'M', 1 + 32, 1 + 32, 1 + 32])
        );
        // SGR-pixels 1016 uses the SGR layout and tells callers to send
        // pixel coordinates.
        screen.process(b"\x1b[?1016h");
        assert!(screen.mouse_pixels());
        assert_eq!(
            screen.mouse_event(MouseKind::Press(MouseButton::Middle), 11, 22, mods),
            Some(b"\x1b[<1;12;23M".to_vec())
        );
        // Resetting a mode that isn't the active encoding leaves it alone.
        screen.process(b"\x1b[?1015l");
        assert!(screen.mouse_pixels());
        screen.process(b"\x1b[?1016l");
        assert!(!screen.mouse_pixels());
    }

    #[test]
    fn decrpm_reports_tracked_modes_and_xtversion_names_the_terminal() {
        let mut screen = TerminalScreen::default();
        screen.process(b"\x1b[?2004h\x1b[?1006h\x1b[?1000h");
        screen.process(b"\x1b[?2004$p\x1b[?1006$p\x1b[?2026$p\x1b[?9999$p\x1b[>q");
        let replies = screen.take_replies();
        assert_eq!(
            replies,
            b"\x1b[?2004;1$y\x1b[?1006;1$y\x1b[?2026;2$y\x1b[?9999;0$y\x1bP>|slate(1)\x1b\\"
        );
    }

    #[test]
    fn xtgettcap_answers_known_caps_and_names_unknown_ones() {
        let mut screen = TerminalScreen::default();
        // "TN", "RGB", "bogus" as hex.
        screen.process(b"\x1bP+q544e;524742;626f677573\x1b\\");
        assert_eq!(
            screen.take_replies(),
            b"\x1bP1+r544e=787465726d2d323536636f6c6f72;524742=382f382f38\x1b\\\x1bP0+r626f677573\x1b\\"
        );
    }

    #[test]
    fn paste_wraps_only_when_the_program_asked() {
        let mut screen = TerminalScreen::default();
        assert_eq!(screen.paste("a\nb"), b"a\rb");
        screen.process(b"\x1b[?2004h");
        assert_eq!(screen.paste("a\x1b[201~b"), b"\x1b[200~a[201~b\x1b[201~");
    }

    #[test]
    fn key_events_report_release_and_repeat_only_with_event_types() {
        let mut screen = TerminalScreen::default();
        let none = KeyMods::default();
        let event = |screen: &TerminalScreen, key, kind| {
            screen.input(&KeyInput::KeyEvent {
                key,
                modifiers: none,
                kind,
            })
        };
        // flags 0: releases are silent, repeats type again.
        assert_eq!(
            event(&screen, TermKey::Char('a'), KeyEventKind::Release),
            None
        );
        assert_eq!(
            event(&screen, TermKey::Char('a'), KeyEventKind::Repeat),
            Some(b"a".to_vec())
        );
        screen.process(b"\x1b[>3u");
        assert_eq!(
            event(&screen, TermKey::Char('j'), KeyEventKind::Release),
            Some(b"\x1b[106;1:3u".to_vec())
        );
        // A repeat of a text key still types text — CSI-u repeat events are
        // for keys the protocol reports as escapes (all-keys below).
        assert_eq!(
            event(&screen, TermKey::Char('j'), KeyEventKind::Repeat),
            Some(b"j".to_vec())
        );
        // Unmodified Enter/Tab/Backspace keep legacy presses and silent
        // releases even with event types on (herdr parity).
        assert_eq!(
            event(&screen, TermKey::Enter, KeyEventKind::Press),
            Some(b"\r".to_vec())
        );
        assert_eq!(event(&screen, TermKey::Enter, KeyEventKind::Release), None);
        screen.process(b"\x1b[<u\x1b[>15u");
        assert_eq!(
            event(&screen, TermKey::Char('j'), KeyEventKind::Repeat),
            Some(b"\x1b[106;1:2u".to_vec())
        );
        // Releases of special keys go out as CSI u events too.
        assert_eq!(
            event(&screen, TermKey::ArrowUp, KeyEventKind::Release),
            Some(b"\x1b[57419;1:3u".to_vec())
        );
    }

    #[test]
    fn kitty_all_keys_alternates_and_associated_text_compose() {
        let mut screen = TerminalScreen::default();
        let none = KeyMods::default();
        let shift = KeyMods {
            shift: true,
            ..none
        };
        let key = |screen: &TerminalScreen, key, modifiers| {
            screen.input(&KeyInput::Key { key, modifiers })
        };
        // flags 9 (disambiguate + all keys): even a plain Enter is CSI u.
        screen.process(b"\x1b[>9u");
        assert_eq!(
            key(&screen, TermKey::Enter, none),
            Some(b"\x1b[13;1u".to_vec())
        );
        assert_eq!(
            key(&screen, TermKey::Char('a'), none),
            Some(b"\x1b[97;1u".to_vec())
        );
        // flags 25 (+ associated text): shift+A reports base codepoint, the
        // modifier, and the text it made.
        screen.process(b"\x1b[<u\x1b[>25u");
        assert_eq!(
            key(&screen, TermKey::Char('A'), shift),
            Some(b"\x1b[97;2;65u".to_vec())
        );
        // flags 31 (+ alternate keys + event types): all fields compose.
        screen.process(b"\x1b[<u\x1b[>31u");
        assert_eq!(
            key(&screen, TermKey::Char('A'), shift),
            Some(b"\x1b[97:65;2:1;65u".to_vec())
        );
        assert_eq!(
            screen.input(&KeyInput::KeyEvent {
                key: TermKey::Char('A'),
                modifiers: shift,
                kind: KeyEventKind::Release,
            }),
            Some(b"\x1b[97:65;2:3u".to_vec())
        );
    }

    #[test]
    fn legacy_modified_specials_keep_their_modifiers() {
        let terminal = TerminalScreen::default();
        let key = |key, modifiers| terminal.input(&KeyInput::Key { key, modifiers });
        let none = KeyMods::default();
        assert_eq!(
            key(TermKey::Delete, KeyMods { alt: true, ..none }),
            Some(b"\x1b[3;3~".to_vec())
        );
        assert_eq!(
            key(
                TermKey::F5,
                KeyMods {
                    shift: true,
                    ..none
                }
            ),
            Some(b"\x1b[15;2~".to_vec())
        );
        assert_eq!(
            key(TermKey::F1, KeyMods { ctrl: true, ..none }),
            Some(b"\x1b[1;5P".to_vec())
        );
        // Ctrl punctuation chords: ^[ is ESC, ^/ ^7 ^_ are 0x1f, ^@ is NUL.
        assert_eq!(
            key(TermKey::Char('['), KeyMods { ctrl: true, ..none }),
            Some(vec![0x1b])
        );
        assert_eq!(
            key(TermKey::Char('/'), KeyMods { ctrl: true, ..none }),
            Some(vec![0x1f])
        );
        assert_eq!(
            key(TermKey::Char('@'), KeyMods { ctrl: true, ..none }),
            Some(vec![0x00])
        );
    }

    #[test]
    fn keypad_keys_follow_application_keypad_mode() {
        let mut terminal = TerminalScreen::default();
        let none = KeyMods::default();
        let key = |terminal: &TerminalScreen, key| {
            terminal.input(&KeyInput::Key {
                key,
                modifiers: none,
            })
        };
        assert_eq!(key(&terminal, TermKey::Numpad('5')), Some(b"5".to_vec()));
        assert_eq!(key(&terminal, TermKey::NumpadEnter), Some(b"\r".to_vec()));
        // DECNKM (ESC =) turns the keypad over to SS3 sequences.
        terminal.process(b"\x1b=");
        assert_eq!(
            key(&terminal, TermKey::Numpad('5')),
            Some(b"\x1bOu".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::Numpad('+')),
            Some(b"\x1bOk".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::NumpadEnter),
            Some(b"\x1bOM".to_vec())
        );
        terminal.process(b"\x1b>");
        assert_eq!(key(&terminal, TermKey::Numpad('5')), Some(b"5".to_vec()));
        // Under kitty all-keys the keypad gets its own codepoints.
        terminal.process(b"\x1b[>9u");
        assert_eq!(
            key(&terminal, TermKey::Numpad('5')),
            Some(b"\x1b[57404;1u".to_vec())
        );
        assert_eq!(
            key(&terminal, TermKey::NumpadEnter),
            Some(b"\x1b[57414;1u".to_vec())
        );
    }

    #[test]
    fn palette_covers_ansi_cube_grayscale_and_truecolor() {
        assert_eq!(color(vt100::Color::Idx(1), (0, 0, 0)), (240, 113, 120));
        assert_eq!(color(vt100::Color::Idx(196), (0, 0, 0)), (255, 0, 0));
        assert_eq!(color(vt100::Color::Idx(255), (0, 0, 0)), (238, 238, 238));
        assert_eq!(color(vt100::Color::Rgb(1, 2, 3), (0, 0, 0)), (1, 2, 3));
    }
}
