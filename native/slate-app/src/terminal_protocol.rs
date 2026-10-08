#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Mode(u16, bool),
    /// ANSI (non-private) `CSI Ps h` / `CSI Ps l` — mode 4 is IRM (insert),
    /// which the orphaned-mode recovery reads. Everything else is emitted but
    /// unconsumed.
    AnsiMode(u16, bool),
    Cursor(u16),
    Reset,
    Status(u16, bool),
    Attributes,
    /// OSC 52 clipboard write — the base64 payload as-is. A `?` or empty
    /// payload is a clipboard *query*, not a write, and never lands here.
    Clipboard(String),
    /// OSC 7 working-directory report — the path already decoded out of the
    /// `file://host/path` URI.
    WorkingDirectory(String),
    /// Kitty keyboard protocol `CSI > flags u`: push a flags entry.
    KittyPush(u8),
    /// Kitty keyboard protocol `CSI < n u`: pop n entries (default 1).
    KittyPop(u16),
    /// Kitty keyboard protocol `CSI ? u`: report the current flags.
    KittyQuery,
    /// Kitty keyboard protocol `CSI = flags ; mode u`: mode 1 assigns the
    /// flags, 2 sets the given bits, 3 clears them (kitty spec, ghostty).
    KittySet(u8, u8),
    /// DECRPM `CSI ? Ps $ p`: the program asks whether a private mode is
    /// set. The screen answers with a DECRPM report (`CSI ? Ps ; Pv $ y`).
    ModeReport(u16),
    /// XTVERSION `CSI > q`: the program asks for the terminal's name and
    /// version; the reply is `DCS > | name(version) ST`.
    VersionReport,
    /// XTGETTCAP `DCS + q Pt ST`: the payload is the raw query — hex-encoded
    /// terminfo capability names separated by `;`. The screen hex-decodes
    /// the names it knows and answers `DCS 1 + r … ST` / `DCS 0 + r … ST`.
    TcapQuery(Vec<u8>),
}

/// The five defined kitty keyboard bits (0b1 disambiguate … 0b10000
/// associated text). A command carrying more is invalid, not truncated.
pub(crate) const KITTY_FLAGS_MAX: u8 = 0b1_1111;

#[derive(Default)]
pub struct Decoder {
    parser: vte::Parser,
    commands: Commands,
}

#[derive(Default)]
struct Commands {
    controls: Vec<Control>,
    /// Payload of the DCS currently being read, when it is one we decode
    /// (XTGETTCAP `DCS + q`). Terminated by `unhook`, never by length.
    dcs: Option<Vec<u8>>,
}

impl vte::Perform for Commands {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        let first = params
            .iter()
            .next()
            .and_then(|p| p.first())
            .copied()
            .unwrap_or(0);
        match (intermediates, action) {
            (b"?", 'h' | 'l') => {
                for param in params.iter().filter(|p| p.len() == 1) {
                    self.controls.push(Control::Mode(param[0], action == 'h'));
                }
            }
            // Plain `CSI Ps h/l` — ANSI modes (4 = IRM). Emitted separately
            // from the DEC-private `Mode` so the two namespaces never mix.
            (b"", 'h' | 'l') => {
                for param in params.iter().filter(|p| p.len() == 1) {
                    self.controls
                        .push(Control::AnsiMode(param[0], action == 'h'));
                }
            }
            // DECRPM: `CSI ? Ps $ p` asks the screen to report a mode.
            (b"?$", 'p') => {
                for param in params.iter().filter(|p| p.len() == 1) {
                    self.controls.push(Control::ModeReport(param[0]));
                }
            }
            // XTVERSION: `CSI > q` (or `CSI > 0 q`) asks who we are.
            (b">", 'q') => self.controls.push(Control::VersionReport),
            (b" ", 'q') if first <= 6 => self.controls.push(Control::Cursor(first)),
            (b"" | b"?", 'n') => self
                .controls
                .push(Control::Status(first, intermediates == b"?")),
            (b"", 'c') if first == 0 => self.controls.push(Control::Attributes),
            // Kitty keyboard protocol. Only the five defined flag bits are
            // valid; a command carrying more is ignored (ghostty's u5 cast).
            (b">", 'u') => {
                let flags = if params.len() == 1 { first } else { 0 };
                if let Ok(flags) = u8::try_from(flags) {
                    if flags <= KITTY_FLAGS_MAX {
                        self.controls.push(Control::KittyPush(flags));
                    }
                }
            }
            (b"<", 'u') => {
                let count = if params.len() == 1 { first.max(1) } else { 1 };
                self.controls.push(Control::KittyPop(count));
            }
            (b"?", 'u') => self.controls.push(Control::KittyQuery),
            (b"=", 'u') => {
                let mut iter = params.iter();
                let flags = iter.next().and_then(|p| p.first()).copied().unwrap_or(0);
                let mode = iter.next().and_then(|p| p.first()).copied().unwrap_or(1);
                if let (Ok(flags), mode @ 1..=3) = (u8::try_from(flags), mode) {
                    if flags <= KITTY_FLAGS_MAX {
                        self.controls.push(Control::KittySet(flags, mode as u8));
                    }
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params.first().copied() {
            // OSC 52 ; selection ; base64 — the program writing the clipboard.
            // An empty payload or `?` queries the clipboard rather than
            // writing it; only real writes are reported.
            Some(b"52") => {
                let Some(Ok(payload)) = params.get(2).map(|p| std::str::from_utf8(p)) else {
                    return;
                };
                if !payload.is_empty() && payload != "?" {
                    self.controls.push(Control::Clipboard(payload.to_owned()));
                }
            }
            // OSC 7 ; file://host/path — the program reporting its cwd.
            Some(b"7") => {
                let Some(Ok(uri)) = params.get(1).map(|p| std::str::from_utf8(p)) else {
                    return;
                };
                if let Some(path) = file_uri_path(uri) {
                    self.controls.push(Control::WorkingDirectory(path));
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.controls.push(Control::Reset);
        }
    }

    // XTGETTCAP arrives as `DCS + q <hex names> ST`: hook selects it, `put`
    // gathers the payload, `unhook` emits the query.
    fn hook(&mut self, _params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        if !ignore && intermediates == b"+" && action == 'q' {
            self.dcs = Some(Vec::new());
        }
    }

    fn put(&mut self, byte: u8) {
        if let Some(dcs) = self.dcs.as_mut() {
            dcs.push(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some(payload) = self.dcs.take() {
            self.controls.push(Control::TcapQuery(payload));
        }
    }
}

impl Decoder {
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<Control> {
        self.parser.advance(&mut self.commands, bytes);
        std::mem::take(&mut self.commands.controls)
    }
}

/// The path in a `file://host/path` URI: the authority is dropped and
/// percent-escapes are decoded. Anything else is not a filesystem path.
fn file_uri_path(uri: &str) -> Option<String> {
    let authority = uri.strip_prefix("file://")?;
    let path = &authority[authority.find('/')?..];
    Some(percent_decode(path))
}

/// `%XX` decoding for URI paths. `+` is a literal plus here — that
/// substitution only exists in `application/x-www-form-urlencoded`.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            hi.and_then(|hi| lo.map(|lo| (hi << 4 | lo) as u8))
        } else {
            None
        };
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_combined_modes_preserve_order() {
        let mut parser = Decoder::default();
        assert!(parser.advance(b"\x1b[?12;10").is_empty());
        assert_eq!(
            parser.advance(b"04l\x1b[5 q\x1b[?12h"),
            vec![
                Control::Mode(12, false),
                Control::Mode(1004, false),
                Control::Cursor(5),
                Control::Mode(12, true),
            ]
        );
        assert!(parser.advance(b"plain").is_empty());
    }

    #[test]
    fn string_payloads_do_not_change_terminal_modes() {
        let mut parser = Decoder::default();
        assert!(parser
            .advance(b"\x1bPtext ?12h\x1b\\\x1b]0;title ?12h\x07")
            .is_empty());
        assert_eq!(parser.advance(b"\x1bc"), vec![Control::Reset]);
    }

    #[test]
    fn osc52_writes_are_captured_but_queries_are_not() {
        let mut parser = Decoder::default();
        assert_eq!(
            parser.advance(b"\x1b]52;c;aGVsbG8=\x07"),
            vec![Control::Clipboard("aGVsbG8=".to_owned())]
        );
        // ST termination works the same as BEL.
        assert_eq!(
            parser.advance(b"\x1b]52;p;d29ybGQ=\x1b\\"),
            vec![Control::Clipboard("d29ybGQ=".to_owned())]
        );
        // `?` reads the clipboard back; an empty payload clears it. Neither
        // is a write.
        assert!(parser.advance(b"\x1b]52;c;?\x07").is_empty());
        assert!(parser.advance(b"\x1b]52;c;\x07").is_empty());
    }

    #[test]
    fn osc52_survives_being_split_across_reads() {
        let mut parser = Decoder::default();
        assert!(parser.advance(b"\x1b]52;c;aGVs").is_empty());
        assert_eq!(
            parser.advance(b"bG8=\x07"),
            vec![Control::Clipboard("aGVsbG8=".to_owned())]
        );
    }

    #[test]
    fn osc7_reports_the_percent_decoded_path() {
        let mut parser = Decoder::default();
        assert_eq!(
            parser.advance(b"\x1b]7;file://hostname/home/user/my%20dir\x07"),
            vec![Control::WorkingDirectory("/home/user/my dir".to_owned())]
        );
        // An empty host is fine; `+` is a literal, not a space.
        assert_eq!(
            parser.advance(b"\x1b]7;file:///a+b\x1b\\"),
            vec![Control::WorkingDirectory("/a+b".to_owned())]
        );
        // Not a file URI: no working directory to report.
        assert!(parser.advance(b"\x1b]7;hostname/path\x07").is_empty());
        assert!(parser.advance(b"\x1b]7;file://host\x07").is_empty());
    }

    #[test]
    fn kitty_keyboard_sequences_decode() {
        let mut parser = Decoder::default();
        assert_eq!(
            parser.advance(b"\x1b[>1u\x1b[<u\x1b[?u\x1b[=3;2u"),
            vec![
                Control::KittyPush(1),
                Control::KittyPop(1),
                Control::KittyQuery,
                Control::KittySet(3, 2),
            ]
        );
        // Push with no flags pushes zero; pop with no count pops one.
        assert_eq!(
            parser.advance(b"\x1b[>u\x1b[<5u"),
            vec![Control::KittyPush(0), Control::KittyPop(5)]
        );
        // Set with no mode defaults to mode 1.
        assert_eq!(parser.advance(b"\x1b[=15u"), vec![Control::KittySet(15, 1)]);
        // Flags beyond the five defined bits, and unknown set modes, are
        // ignored rather than truncated.
        assert!(parser.advance(b"\x1b[>32u\x1b[=1;9u").is_empty());
    }

    #[test]
    fn decrpm_xtversion_and_xtgettcap_decode() {
        let mut parser = Decoder::default();
        assert_eq!(
            parser.advance(b"\x1b[?2026$p\x1b[>q"),
            vec![Control::ModeReport(2026), Control::VersionReport]
        );
        // XTGETTCAP: `DCS + q` then hex-encoded capability names, ST ends it.
        assert_eq!(
            parser.advance(b"\x1bP+q544e;524742\x1b\\"),
            vec![Control::TcapQuery(b"544e;524742".to_vec())]
        );
        // Only ST ends a DCS — a BEL inside the string is payload. The
        // pending query is still delivered when the next escape opens.
        assert!(parser.advance(b"\x1bP+q434f\x07").is_empty());
        assert_eq!(
            parser.advance(b"\x1bP+q544e\x1b\\"),
            vec![
                Control::TcapQuery(b"434f\x07".to_vec()),
                Control::TcapQuery(b"544e".to_vec())
            ]
        );
        // A different DCS hook carries no query we answer.
        assert!(parser.advance(b"\x1bP$r\"p\x1b\\").is_empty());
        // The query survives being split across reads.
        assert!(parser.advance(b"\x1bP+q54").is_empty());
        assert_eq!(
            parser.advance(b"4e\x1b\\"),
            vec![Control::TcapQuery(b"544e".to_vec())]
        );
    }
}
