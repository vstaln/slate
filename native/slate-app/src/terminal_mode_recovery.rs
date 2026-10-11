//! Port of the shell's `terminalModeRecovery.ts` — getting a terminal back out
//! of modes a dead application left it in.
//!
//! A full-screen agent turns on mouse reporting (and bracketed paste, and the
//! alternate buffer) on its way up, and turns them off again on its way out.
//! An agent that *crashes* never runs that second half, so the shell it drops
//! back to inherits every one of them without having asked for any. The mode
//! that makes the terminal unusable is mouse tracking: the emulator keeps
//! reporting pointer movement, and with no application left to read the
//! reports the shell echoes each one into the prompt —
//!
//!   ^[[<35;36;37M^[[<35;37;37M^[[<35;38;36M…
//!
//! There is no process exit to hook here: the pty is alive and well, it is the
//! thing that was *running inside it* that died. So the state is read back out
//! of the output instead, by two independent signals: echoed mouse reports in
//! the stream, and console-title transitions naming the shell's foreground
//! child. Either is enough to justify re-asserting the modes a bare shell
//! actually wants.

use crate::terminal_screen::TerminalScreen;
use std::time::{Duration, Instant};

/// The private modes an application-owned screen gets put back out of —
/// `APP_OWNED_MODE_RESET` in `shared/terminalModes.ts`, byte for byte: every
/// mouse reporting and focus mode (9, 1000-1003, 1004) and the UTF-8/SGR/urxvt
/// encodings (1005, 1006, 1015), bracketed paste (2004), synchronized output
/// (2026), the alternate screen (1049), cursor visible (25), autowrap on (7),
/// insert mode off (4), then origin mode and the scrolling region reset inside
/// a DECSC/DECRC pair so the cursor returns to the row the shell is writing
/// on instead of being homed to row 1.
pub const APP_OWNED_MODE_RESET: &str = "\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?2026l\x1b[?1049l\x1b[?25h\x1b[?7h\x1b[4l\x1b7\x1b[?6l\x1b[r\x1b8";

/// `EXIT_CONFIRM_MS` — a bare shell title is only an exit if nothing
/// supersedes it; launcher shims hand the title back on their way to starting
/// the real process.
pub const EXIT_CONFIRM: Duration = Duration::from_millis(1_500);
/// `INTERRUPT_RECOVERY_DELAY_MS` — the kill is asynchronous: SIGINT has to
/// reach the child, the TUI has to die, and its last frame has to parse before
/// the emulator state reads settled.
pub const INTERRUPT_DELAY: Duration = Duration::from_millis(400);
/// `INTERRUPT_RECOVERY_RETRY_MS` — a check that lands mid-teardown retries.
pub const INTERRUPT_RETRY: Duration = Duration::from_millis(120);
/// `INTERRUPT_RECOVERY_MAX_MS` — a TUI that is still up after this long
/// handled the keystroke itself; stop watching.
pub const INTERRUPT_MAX: Duration = Duration::from_millis(5_000);

/// How many *adjacent* echoed reports have to be seen before the terminal is
/// reset. Adjacency is what separates a burst from a report an agent merely
/// printed in prose.
const MOUSE_ECHO_THRESHOLD: usize = 3;
/// How far apart two reports may sit and still count as back to back — a shell
/// that wraps the line inserts a CRLF between them, which is still one burst.
const ADJACENT_GAP: usize = 2;
/// Longest echoed report, so a match split across two chunks is still seen.
const CARRY_LIMIT: usize = 24;
/// A console title is short; this bounds what an unterminated one can hold.
const TITLE_CARRY_LIMIT: usize = 512;

/// What the console titles in one chunk say about the shell's foreground
/// child — the *transition* is what counts, not the title on its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildTitle {
    Started,
    Exited,
}

/// Per-terminal probe state — the shell's `ModeRecoveryProbe`.
#[derive(Debug, Default)]
pub struct ModeRecoveryProbe {
    /// Tail of the previous chunk, re-scanned so no report straddles a
    /// boundary.
    carry: String,
    /// The carry continues a burst rather than interrupting it.
    contiguous: bool,
    /// Reports counted in the current uninterrupted burst.
    hits: usize,
    /// A console title has named a child, so there is an exit to wait for.
    saw_child: bool,
    /// Unterminated tail of the previous chunk; a title can straddle two.
    title_carry: String,
}

/// Strip escape sequences with no replacement — what remains is the text the
/// shell actually drew. PSReadLine repaints the input line with syntax
/// highlighting, so a single echoed report arrives interleaved with colour
/// codes; taking them out is what puts two reports back next to each other.
fn strip_escapes(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            // OSC — `ESC ] … (BEL | ESC \)`. Unterminated, the original's
            // pattern falls back to the two-byte `ESC ]` match.
            Some(b']') => {
                let mut j = i + 2;
                let mut terminated = false;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        j += 1;
                        terminated = true;
                        break;
                    }
                    if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                        j += 2;
                        terminated = true;
                        break;
                    }
                    j += 1;
                }
                i = if terminated { j } else { i + 2 };
            }
            // CSI — `ESC [ [0-?]* [ -/]* [@-~]`. Unterminated falls back to
            // the two-byte `ESC [` match, leaving the parameters as text.
            Some(b'[') => {
                let mut j = i + 2;
                while j < bytes.len() && (0x30..=0x3f).contains(&bytes[j]) {
                    j += 1;
                }
                while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                    j += 1;
                }
                if j < bytes.len() && (0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                    i = j;
                } else {
                    i += 2;
                }
            }
            // `ESC Fe` — the two-byte escapes; the class includes `[` and `]`
            // themselves, which the arms above already claimed when they could.
            Some(&next) if (0x40..=0x5f).contains(&next) => i += 2,
            // A lone ESC at the end of the buffer or before a non-escape byte.
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(?:\^\[)?\[<?\d{1,4};\d{1,5};\d{1,5}[Mm]` — a mouse report as a shell
/// *rendered* it: cmd keeps the caret notation (`^[[<35;36;37M`), PowerShell
/// drops both markers (`[555;87;30M`). Returns the match's byte length.
fn match_report(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = start;
    if bytes.get(i) == Some(&b'^') {
        if bytes.get(i + 1) == Some(&b'[') {
            i += 2;
        } else {
            return None;
        }
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    if bytes.get(i) == Some(&b'<') {
        i += 1;
    }
    let mut digits = 0;
    while digits < 4 && bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    for _ in 0..2 {
        if bytes.get(i) != Some(&b';') {
            return None;
        }
        i += 1;
        digits = 0;
        while digits < 5 && bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
    }
    match bytes.get(i) {
        Some(b'M') | Some(b'm') => Some(i + 1 - start),
        _ => None,
    }
}

/// The start of a report that has not arrived in full yet — `^[[<35;` or
/// `[555;87` sits between two reports, so a burst split across a transport
/// boundary has to stay one burst. Anything else breaks it.
fn is_partial_report(tail: &str) -> bool {
    let bytes = tail.as_bytes();
    let mut i = 0;
    if bytes.starts_with(b"^[") {
        i = 2;
    }
    if bytes.get(i) == Some(&b'[') {
        i += 1;
    }
    if bytes.get(i) == Some(&b'<') {
        i += 1;
    }
    bytes[i..].iter().all(|b| b.is_ascii_digit() || *b == b';')
}

/// `ESC ] 0 ; <title> (BEL | ESC \)` — the console titles set in `text`,
/// oldest first. A sequence with no terminator is no title at all, matching
/// the original's pattern which requires the BEL or ST.
fn terminal_titles(text: &str) -> Vec<&str> {
    let mut titles = Vec::new();
    let mut offset = 0;
    while let Some(open) = text[offset..].find("\x1b]0;") {
        let start = offset + open + 4;
        let rest = &text[start..];
        let bytes = rest.as_bytes();
        let mut i = 0;
        let mut terminated = None;
        while i < bytes.len() {
            match bytes[i] {
                0x07 => {
                    terminated = Some((i, i + 1));
                    break;
                }
                // ST ends it; a bare ESC just fails the match.
                0x1b => {
                    if bytes.get(i + 1) == Some(&b'\\') {
                        terminated = Some((i, i + 2));
                    }
                    break;
                }
                _ => i += 1,
            }
        }
        match terminated {
            Some((end, after)) => {
                titles.push(&rest[..end]);
                offset = start + after;
            }
            None => offset = start,
        }
    }
    titles
}

/// A title that names a shell and nothing else — whatever ran under it is
/// gone. `(?:^|[\\/])(?:cmd|powershell|pwsh|bash|zsh|sh|fish)(?:\.exe)?$` or
/// `^windows powershell$`.
fn is_shell_own_title(title: &str) -> bool {
    let value = title.trim().to_lowercase();
    if value.is_empty() {
        return false;
    }
    if value == "windows powershell" {
        return true;
    }
    let base = value.rsplit(['/', '\\']).next().unwrap_or(&value);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    matches!(
        base,
        "cmd" | "powershell" | "pwsh" | "bash" | "zsh" | "sh" | "fish"
    )
}

impl ModeRecoveryProbe {
    /// Feed one chunk of terminal output; `true` means the shell is echoing
    /// mouse reports and the emulator has to be taken out of mouse tracking.
    /// The probe resets its count on a positive verdict, so a caller that
    /// resets the terminal and keeps feeding gets one verdict per burst.
    pub fn read_echoed_mouse_reports(&mut self, chunk: &str) -> bool {
        let scanned = if self.carry.is_empty() {
            strip_escapes(chunk)
        } else {
            strip_escapes(&format!("{}{chunk}", self.carry))
        };
        if !scanned.contains('[') {
            self.carry.clear();
            self.contiguous = false;
            self.hits = 0;
            return false;
        }
        // A contiguous carry stands in for the report that ended just before
        // the chunk boundary, so a burst interrupted only by the transport
        // keeps its count.
        let mut previous_end: Option<usize> = self.contiguous.then_some(0);
        let mut consumed = 0;
        let mut found = false;
        let mut position = 0;
        while position < scanned.len() {
            if !matches!(scanned.as_bytes()[position], b'[' | b'^') {
                position += 1;
                continue;
            }
            let Some(length) = match_report(&scanned, position) else {
                position += 1;
                continue;
            };
            let start = position;
            let end = start + length;
            // A report that touches the previous one continues the burst; one
            // standing alone in other text starts a new burst of length 1.
            // The gap counts characters, the unit the original's JS string
            // indexes measure.
            let adjacent = previous_end.is_some_and(|end_of_last| {
                scanned[end_of_last..start].chars().count() <= ADJACENT_GAP
            });
            self.hits = if adjacent { self.hits + 1 } else { 1 };
            previous_end = Some(end);
            consumed = end;
            if self.hits >= MOUSE_ECHO_THRESHOLD {
                self.hits = 0;
                previous_end = None;
                found = true;
            }
            position = end;
        }
        // Carry only what follows the last complete report, so a counted
        // report can never be counted again.
        let trailing = &scanned[consumed..];
        self.carry = trailing
            .chars()
            .rev()
            .take(CARRY_LIMIT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.contiguous = previous_end.is_some()
            && (trailing.chars().count() <= ADJACENT_GAP || is_partial_report(trailing));
        if !self.contiguous {
            self.hits = 0;
        }
        found
    }

    /// What this chunk's console titles say about the shell's foreground
    /// child, or `None` when they say nothing new. Runs for the life of the
    /// terminal: an agent can crash at any point, and the mode it leaves
    /// behind is just as unusable an hour in as it is at startup.
    pub fn read_child_title(&mut self, chunk: &str) -> Option<ChildTitle> {
        let scanned = if self.title_carry.is_empty() {
            chunk.to_owned()
        } else {
            format!("{}{chunk}", self.title_carry)
        };
        // Keep only what follows the last terminated sequence, so no title is
        // read twice, and cap it because a console title is short. The
        // original slices from the index *after* the last terminator's first
        // byte — a leftover `\` at the carry's head is inert to title scans.
        let last_bel = scanned.rfind('\x07').map(|i| i + 1);
        let last_st = scanned.rfind("\x1b\\").map(|i| i + 1);
        let last_end = last_bel.max(last_st).unwrap_or(0);
        let tail = &scanned[last_end..];
        self.title_carry = tail
            .chars()
            .rev()
            .take(TITLE_CARRY_LIMIT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        // A tail with no escape and no BEL can never start or finish a title;
        // holding 512 chars of plain prompt output forever would defeat the
        // watcher's no-escape fast path on every chunk.
        if !self.title_carry.contains('\x1b') && !self.title_carry.contains('\x07') {
            self.title_carry.clear();
        }
        if !scanned.contains("\x1b]0;") {
            return None;
        }
        let mut event = None;
        for title in terminal_titles(&scanned) {
            if is_shell_own_title(title) {
                if self.saw_child {
                    self.saw_child = false;
                    event = Some(ChildTitle::Exited);
                }
            } else {
                // Titles inside one chunk are already in order, so a later one
                // wins: a launcher shim restoring the shell title on its way to
                // starting the agent must not read as an exit.
                if !self.saw_child {
                    event = Some(ChildTitle::Started);
                }
                self.saw_child = true;
            }
        }
        event
    }
}

/// The whole recovery driver for one terminal — the probe plus the two
/// pending timers the shell kept in `TerminalWidget`. The engine feeds every
/// output chunk through [`ModeRecovery::watch`] and calls
/// [`ModeRecovery::tick`] on its periodic pass.
#[derive(Debug, Default)]
pub struct ModeRecovery {
    probe: ModeRecoveryProbe,
    /// A console-title "child exited" is held for [`EXIT_CONFIRM`] — a
    /// launcher shim hands the title back on its way to the real process, so
    /// a bare title is only an exit if nothing supersedes it.
    exit_at: Option<Instant>,
    /// The corpse-state check runs at this instant — armed when a Ctrl-C /
    /// Ctrl-D went to a terminal whose application may not have unwound.
    interrupt_at: Option<Instant>,
    /// Retries stop here; a TUI still up after it handled the key itself.
    interrupt_deadline: Option<Instant>,
}

impl ModeRecovery {
    /// `watchOrphanedModes` — every chunk the pty produces. The verdict is
    /// `true` when the shell is echoing mouse reports right now; the caller
    /// applies the reset immediately after the chunk has parsed. Title
    /// verdicts update the driver's own timers instead.
    pub fn watch(&mut self, chunk: &str) -> bool {
        // Fast path for the overwhelmingly common case: with no probe state
        // pending and no escape or bracket in the chunk, neither signal can
        // fire — both probes would be provable no-ops. This keeps the
        // per-chunk scan cost off the PTY fast path.
        if self.probe.carry.is_empty()
            && self.probe.hits == 0
            && self.probe.title_carry.is_empty()
            && !chunk.contains('\x1b')
            && !chunk.contains('[')
        {
            return false;
        }
        // Both probes see every chunk: the echo one has to, to count a burst,
        // and the title one, to track the child.
        let echoed = self.probe.read_echoed_mouse_reports(chunk);
        match self.probe.read_child_title(chunk) {
            Some(ChildTitle::Started) => {
                // Something is running again — neither a pending recovery nor
                // an armed interrupt check still describes this terminal.
                self.exit_at = None;
                self.interrupt_at = None;
                self.interrupt_deadline = None;
            }
            Some(ChildTitle::Exited) => {
                self.exit_at = Some(Instant::now() + EXIT_CONFIRM);
            }
            None => {}
        }
        echoed
    }

    /// The echo verdict's reset — `recoverModes` in the shell. A terminal on
    /// the alternate buffer is still owned by a full-screen application (the
    /// check runs only after output settled, so a dying TUI's own `1049l`
    /// already landed), and is left alone.
    pub fn recover(&mut self, screen: &mut TerminalScreen) {
        if screen.alternate_screen() {
            return;
        }
        if screen.has_orphaned_modes() {
            screen.process(APP_OWNED_MODE_RESET.as_bytes());
        }
        self.settle(screen);
    }

    /// `markInterruptRecoveryPending` + `armInterruptRecovery` — a Ctrl-C or
    /// Ctrl-D may have killed the foreground TUI without cleanup, leaving
    /// origin mode or the scroll region behind. The check only acts on the
    /// self-evident corpse state, so a TUI that handled the key itself is
    /// never disturbed.
    pub fn arm_interrupt(&mut self, screen: &mut TerminalScreen) {
        let now = Instant::now();
        self.interrupt_deadline = Some(now + INTERRUPT_MAX);
        self.interrupt_at = Some(now + INTERRUPT_DELAY);
        // The prompt may be rendered above the old viewport in the interim;
        // park at the live bottom now rather than after the check.
        if !screen.alternate_screen() {
            screen.scroll_to_bottom();
        }
    }

    /// Periodic driver for both timers — due exit confirmations run the reset
    /// check, due interrupt checks run the corpse-state pass.
    pub fn tick(&mut self, screen: &mut TerminalScreen) {
        let now = Instant::now();
        if self.exit_at.is_some_and(|at| now >= at) {
            self.exit_at = None;
            self.recover(screen);
        }
        if self.interrupt_at.is_some_and(|at| now >= at) {
            self.interrupt_at = None;
            // Back on the normal buffer with mouse tracking off is the only
            // state the check may act on; a TUI mid-teardown is retried until
            // the deadline, and a still-running one is never touched.
            if screen.alternate_screen() || screen.mouse_reporting() {
                match self.interrupt_deadline {
                    Some(deadline) if now < deadline => {
                        self.interrupt_at = Some(now + INTERRUPT_RETRY.min(deadline - now));
                    }
                    _ => self.interrupt_deadline = None,
                }
                return;
            }
            if screen.has_orphaned_modes() {
                screen.process(APP_OWNED_MODE_RESET.as_bytes());
            }
            self.interrupt_deadline = None;
            screen.scroll_to_bottom();
        }
    }

    /// Recovery finished — the viewport parks at the live bottom so the next
    /// prompt line is visible instead of the stale top lines.
    fn settle(&mut self, screen: &mut TerminalScreen) {
        self.interrupt_at = None;
        self.interrupt_deadline = None;
        screen.scroll_to_bottom();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echoed(probe: &mut ModeRecoveryProbe, chunk: &str) -> bool {
        probe.read_echoed_mouse_reports(chunk)
    }

    const CARET: &str = "^[[<35;36;37M^[[<35;37;37M^[[<35;38;36M";

    #[test]
    fn reports_a_caret_notation_burst() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(echoed(&mut probe, CARET));
    }

    #[test]
    fn ignores_reports_arriving_as_real_escape_sequences() {
        // A report with its ESC intact is consumed by the parser as a control
        // sequence — nothing reached the screen, so there is nothing to fix.
        let mut probe = ModeRecoveryProbe::default();
        assert!(!echoed(
            &mut probe,
            "\x1b[<35;36;37M\x1b[<35;37;37M\x1b[<35;38;36M"
        ));
    }

    #[test]
    fn stays_quiet_for_a_single_printed_sequence() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(!echoed(&mut probe, "the report looks like ^[[<35;36;37M"));
    }

    #[test]
    fn stays_quiet_for_reports_in_prose() {
        let mut probe = ModeRecoveryProbe::default();
        let prose = "a press is ^[[<0;10;5M, a release ^[[<0;10;5m, and motion ^[[<35;36;37M.\r\n";
        for _ in 0..20 {
            assert!(!echoed(&mut probe, prose));
        }
    }

    #[test]
    fn counts_a_burst_that_wrapped_across_lines() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(echoed(
            &mut probe,
            "^[[<35;36;37M\r\n^[[<35;37;37M\r\n^[[<35;38;36M"
        ));
    }

    #[test]
    fn stays_quiet_for_ordinary_agent_output() {
        let mut probe = ModeRecoveryProbe::default();
        let banner = "\x1b[38;2;237;237;237mopencode\x1b[m  Ask anything…\r\n";
        for _ in 0..40 {
            assert!(!echoed(&mut probe, banner));
        }
    }

    #[test]
    fn accumulates_reports_one_chunk_at_a_time() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(!echoed(&mut probe, "^[[<35;36;37M"));
        assert!(!echoed(&mut probe, "^[[<35;37;37M"));
        assert!(echoed(&mut probe, "^[[<35;38;36M"));
    }

    #[test]
    fn sees_a_report_split_across_a_transport_boundary() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(!echoed(&mut probe, "^[[<35;36;37M^[[<35;37;37M^[[<35;"));
        assert!(echoed(&mut probe, "38;36M"));
    }

    #[test]
    fn does_not_count_the_same_burst_twice() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(echoed(&mut probe, CARET));
        assert!(!echoed(&mut probe, "^[[<35;39;36M"));
    }

    fn ps_report(x: u32, y: u32) -> String {
        format!("[555\x1b[0m\x1b[37m;\x1b[0m\x1b[97m{x}\x1b[0m\x1b[37m;\x1b[0m\x1b[93m{y}M")
    }

    #[test]
    fn reports_a_powershell_highlighted_burst() {
        let mut probe = ModeRecoveryProbe::default();
        let burst = format!(
            "{}{}{}",
            ps_report(87, 30),
            ps_report(88, 31),
            ps_report(89, 32)
        );
        assert!(echoed(&mut probe, &burst));
    }

    #[test]
    fn counts_a_powershell_burst_split_across_chunks() {
        let mut probe = ModeRecoveryProbe::default();
        assert!(!echoed(&mut probe, &ps_report(87, 30)));
        assert!(!echoed(&mut probe, &ps_report(88, 31)));
        assert!(echoed(&mut probe, &ps_report(89, 32)));
    }

    #[test]
    fn stays_quiet_for_a_powershell_prompt() {
        let mut probe = ModeRecoveryProbe::default();
        let prompt = "\x1b[0m\x1b[37mPS \x1b[0m\x1b[97mC:\\Users\\user\x1b[0m> ";
        for _ in 0..20 {
            assert!(!echoed(&mut probe, prompt));
        }
    }

    #[test]
    fn stays_quiet_for_a_coloured_tui_frame() {
        let mut probe = ModeRecoveryProbe::default();
        let frame = "\x1b[38;2;120;160;255m\x1b[48;5;236m   \x1b[1;31;42m\x1b[2J\x1b[10;20H";
        for _ in 0..40 {
            assert!(!echoed(&mut probe, frame));
        }
    }

    #[test]
    fn recognises_a_shell_own_title() {
        for title in [
            "C:\\WINDOWS\\system32\\cmd.exe",
            "cmd.exe",
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
            "Windows PowerShell",
            "/bin/bash",
        ] {
            assert!(is_shell_own_title(title), "{title}");
        }
        for title in ["C:\\WINDOWS\\system32\\cmd.exe - opencode", "opencode", ""] {
            assert!(!is_shell_own_title(title), "{title}");
        }
    }

    const SHELL: &str = "C:\\WINDOWS\\system32\\cmd.exe";

    fn title(value: &str) -> String {
        format!("\x1b]0;{value}\x07")
    }

    #[test]
    fn reports_the_shell_taking_its_title_back() {
        let mut probe = ModeRecoveryProbe::default();
        assert_eq!(
            probe.read_child_title(&title(&format!("{SHELL} - opencode"))),
            Some(ChildTitle::Started)
        );
        assert_eq!(
            probe.read_child_title(&title(SHELL)),
            Some(ChildTitle::Exited)
        );
    }

    #[test]
    fn ignores_a_shell_that_never_ran_anything() {
        let mut probe = ModeRecoveryProbe::default();
        for _ in 0..5 {
            assert_eq!(probe.read_child_title(&title("Windows PowerShell")), None);
        }
    }

    #[test]
    fn reports_each_exit_once() {
        let mut probe = ModeRecoveryProbe::default();
        probe.read_child_title(&title(&format!("{SHELL} - opencode")));
        assert_eq!(
            probe.read_child_title(&title(SHELL)),
            Some(ChildTitle::Exited)
        );
        assert_eq!(probe.read_child_title(&title(SHELL)), None);
    }

    #[test]
    fn reads_titles_inside_one_chunk_in_order() {
        let mut probe = ModeRecoveryProbe::default();
        let chunk = format!(
            "{}{}{}",
            title(&format!("{SHELL} - codex")),
            title(SHELL),
            title(&format!("{SHELL} - node"))
        );
        assert_eq!(probe.read_child_title(&chunk), Some(ChildTitle::Started));
    }

    #[test]
    fn joins_a_title_split_across_two_chunks() {
        // One probe across both halves — the exit only reads as an exit
        // because the child title was seen first.
        let mut probe = ModeRecoveryProbe::default();
        let started = title("OpenCode");
        assert_eq!(probe.read_child_title(&started[..6]), None);
        assert_eq!(
            probe.read_child_title(&started[6..]),
            Some(ChildTitle::Started)
        );
        let exited = title("C:\\WINDOWS\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        assert_eq!(probe.read_child_title(&exited[..20]), None);
        assert_eq!(
            probe.read_child_title(&exited[20..]),
            Some(ChildTitle::Exited)
        );
    }

    #[test]
    fn never_reads_the_same_title_twice() {
        let mut probe = ModeRecoveryProbe::default();
        assert_eq!(
            probe.read_child_title(&title("OpenCode")),
            Some(ChildTitle::Started)
        );
        assert_eq!(probe.read_child_title("plain output, no titles"), None);
        assert_eq!(
            probe.read_child_title(&title(SHELL)),
            Some(ChildTitle::Exited)
        );
        assert_eq!(probe.read_child_title("more plain output"), None);
    }

    #[test]
    fn the_carry_never_grows_on_unterminated_output() {
        let mut probe = ModeRecoveryProbe::default();
        for _ in 0..50 {
            probe.read_child_title(&"x".repeat(400));
        }
        assert!(probe.title_carry.len() <= TITLE_CARRY_LIMIT);
    }

    #[test]
    fn reads_a_replayed_history_as_one_chunk() {
        // Launcher shims first, the agent, then the exit — the only record a
        // live re-attach has of a crash the new process never saw.
        let mut probe = ModeRecoveryProbe::default();
        let history = format!(
            "{}{}{}\x1b[?1049h\x1b[?1003h\x1b[?1006hopencode painting its screen…{}",
            title(&format!("{SHELL} - chcp")),
            title(SHELL),
            title(&format!("{SHELL} - opencode")),
            title(SHELL)
        );
        assert_eq!(probe.read_child_title(&history), Some(ChildTitle::Exited));
    }

    #[test]
    fn does_not_read_a_healthy_session_as_an_exit() {
        let mut probe = ModeRecoveryProbe::default();
        let history = format!(
            "{}{}\x1b[?1049h",
            title(SHELL),
            title(&format!("{SHELL} - opencode"))
        );
        assert_eq!(probe.read_child_title(&history), Some(ChildTitle::Started));
    }

    #[test]
    fn orphaned_modes_match_the_emulator_state() {
        let mut screen = TerminalScreen::default();
        assert!(!screen.has_orphaned_modes());
        screen.process(b"\x1b[?1003h");
        assert!(screen.has_orphaned_modes());
        screen.process(APP_OWNED_MODE_RESET.as_bytes());
        assert!(!screen.has_orphaned_modes());
        // Origin mode, insert mode and a dropped autowrap each count on their
        // own — the corpse states a clean `1049l` can still leave behind.
        screen.process(b"\x1b[?6h");
        assert!(screen.has_orphaned_modes());
        screen.process(APP_OWNED_MODE_RESET.as_bytes());
        assert!(!screen.has_orphaned_modes());
        screen.process(b"\x1b[4h");
        assert!(screen.has_orphaned_modes());
        screen.process(APP_OWNED_MODE_RESET.as_bytes());
        assert!(!screen.has_orphaned_modes());
        screen.process(b"\x1b[?7l");
        assert!(screen.has_orphaned_modes());
        screen.process(APP_OWNED_MODE_RESET.as_bytes());
        assert!(!screen.has_orphaned_modes());
    }

    #[test]
    fn recovery_leaves_the_alternate_buffer_alone() {
        let mut recovery = ModeRecovery::default();
        let mut screen = TerminalScreen::default();
        screen.process(b"\x1b[?1049h\x1b[?1003h");
        recovery.recover(&mut screen);
        assert!(screen.alternate_screen());
        assert!(screen.mouse_reporting());
    }
}
