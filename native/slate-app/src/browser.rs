//! Browser-widget URL handling — a port of the shell's
//! `renderer/src/lib/browserShared.ts` plus the stricter control-server
//! normalizer and checkout guard from `main/controlServer.ts`.
//!
//! Two normalizers exist on purpose, same as upstream:
//!
//! * [`to_navigation_url`] is the address-bar grammar: URLs, `file://`, bare
//!   OS paths, `localhost:3000`, domains — and anything else falls through to
//!   a Google search. User-driven, permissive.
//! * [`browser_navigation_url`] is the control-route grammar: http(s) only,
//!   no whitespace, no credentials, localhost allowed. Programmatic callers
//!   (agents) never get a silent Google search.
//!
//! [`is_protected_checkout_url`] mirrors `PROTECTED_CHECKOUT_PATH` +
//! `isProtectedCheckoutUrl` from `shared/browserActionScripts.ts` /
//! `controlServer.ts`: checkout/payment surfaces are off-limits to
//! programmatic navigation. The original never applied it to address-bar
//! browsing, and neither do we.

/// browserShared.ts `HOME_URL` — the widget's landing page. The home screen
/// is rendered by the pane itself; the webview sits behind it on
/// `about:blank` so the guest is ready the moment an address is entered.
pub const HOME_URL: &str = "https://www.google.com";

/// RFC 3986 unreserved + the path-safe punctuation `URL` kept verbatim.
fn push_pct_encoded(out: &mut String, raw: &str) {
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b'-'
            | b'.'
            | b'/'
            | b':'
            | b';'
            | b'='
            | b'@'
            | b'_'
            | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
}

/// `new URL(raw).href` stand-in for the checks the original leaned on the JS
/// parser for: no whitespace, no userinfo credentials, and (unless the scheme
/// legitimately has no host, like `file:`) a non-empty authority.
fn url_shape_ok(raw: &str, allow_empty_host: bool) -> bool {
    if raw.contains(char::is_whitespace) {
        return false;
    }
    let after_scheme = match raw.split_once("://") {
        Some((_, rest)) => rest,
        // `file:relative` and bare `file:x` forms — treat the rest as opaque.
        None => return raw.to_lowercase().starts_with("file:"),
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    if !allow_empty_host
        && authority
            .split(':')
            .next()
            .unwrap_or_default()
            .is_empty()
    {
        return false;
    }
    true
}

/// Percent-encode a `file:` URL the way `new URL(raw).href` did — the scheme
/// and authority separators stay, unsafe bytes become `%XX`.
fn normalize_file_url(raw: &str) -> Option<String> {
    if !(raw.len() >= 5 && raw[..5].eq_ignore_ascii_case("file:")) {
        return None;
    }
    let rest = &raw[5..];
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let mut out = String::with_capacity(rest.len() + 5);
    out.push_str("file:");
    push_pct_encoded(&mut out, rest);
    Some(out)
}

/// `localPathToFileUrl` — `C:\dir\page.html`, `C:/…`, `\\server\share\…`, and
/// (POSIX only) `/abs/path`. Returns a normalized `file:` URL.
fn local_path_to_file_url(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let is_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let is_unc =
        raw.starts_with("\\\\") && bytes.get(2).is_some_and(|b| *b != b'\\' && *b != b'/');
    // POSIX absolute paths only resolve where POSIX paths exist. Mirrors the
    // original's `!IS_WINDOWS && /^\/[^\s]*[/.][^\s]*$/` — a `/` or `.` must
    // appear after the leading slash so `/word` stays a search.
    let is_posix = !cfg!(windows)
        && bytes.first() == Some(&b'/')
        && bytes[1..].iter().any(|b| *b == b'/' || *b == b'.')
        && !raw.contains(char::is_whitespace);
    if !is_drive && !is_unc && !is_posix {
        return None;
    }
    let slashed: String = raw.chars().map(|c| if c == '\\' { '/' } else { c }).collect();
    let mut out = String::with_capacity(slashed.len() + 8);
    if is_unc {
        // `//server/share/…` keeps its authority: file://server/share/…
        out.push_str("file:");
        push_pct_encoded(&mut out, &slashed);
    } else {
        out.push_str("file:///");
        push_pct_encoded(&mut out, slashed.trim_start_matches('/'));
    }
    Some(out)
}

/// `/^(localhost|127\.0\.0\.1|0\.0\.0\.0)(:\d+)?([/?#]|$)/i`
fn is_loopback_hostish(lower: &str) -> bool {
    let host = lower.split(['/', '?', '#']).next().unwrap_or("");
    let (name, port_ok) = match host.split_once(':') {
        Some((n, p)) => (n, !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        None => (host, true),
    };
    port_ok && matches!(name, "localhost" | "127.0.0.1" | "0.0.0.0")
}

/// `/^[\w-]+(\.[\w-]+)+(:\d+)?([/?#]|$)/` — the address bar's "dotted host"
/// rule: at least two alphanumeric/dash/underscore labels.
fn looks_like_domain(raw: &str) -> bool {
    let host = raw.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port_ok) = match host.split_once(':') {
        Some((h, p)) => (h, !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        None => (host, true),
    };
    port_ok
        && host.split('.').count() >= 2
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        })
}

/// `toNavigationUrl` — the address bar's grammar.
pub fn to_navigation_url(input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    let compact: String = raw.split_whitespace().collect::<String>().to_lowercase();
    if compact.starts_with("javascript:")
        || compact.starts_with("data:")
        || compact.starts_with("vbscript:")
    {
        return None;
    }
    if raw.len() >= 5 && raw[..5].eq_ignore_ascii_case("file:") {
        return normalize_file_url(raw).filter(|url| url_shape_ok(url, true));
    }
    if let Some(local) = local_path_to_file_url(raw) {
        return Some(local);
    }
    let lower = raw.to_lowercase();
    let candidate = if lower.starts_with("http://") || lower.starts_with("https://") {
        Some(raw.to_owned())
    } else if is_loopback_hostish(&lower) {
        Some(format!("http://{raw}"))
    } else if !raw.contains(char::is_whitespace) && looks_like_domain(raw) {
        Some(format!("https://{raw}"))
    } else {
        None
    };
    if let Some(candidate) = candidate {
        if url_shape_ok(&candidate, false) {
            return Some(candidate);
        }
        return None;
    }
    Some(search_url(raw))
}

/// Searches land on Google, same query shape the original built
/// (`encodeURIComponent` over the raw input).
fn search_url(query: &str) -> String {
    let mut encoded = String::with_capacity(query.len() * 2);
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    format!("https://www.google.com/search?q={encoded}")
}

/// `browserNavigationUrl` — the control server's stricter grammar: the raw
/// string must be whitespace-free and ≤2048 chars; an explicit `http(s)://`
/// prefix passes through verbatim, loopback gets `http://`, and everything
/// else that looks like a bare host gets `https://`. Credentials in the
/// authority are rejected — `parsed.username || parsed.password` upstream.
pub fn browser_navigation_url(value: &str) -> Option<String> {
    let raw = value.trim();
    if raw.is_empty() || raw.len() > 2048 || raw.contains(char::is_whitespace) {
        return None;
    }
    let lower = raw.to_lowercase();
    let candidate = if lower.starts_with("http://") || lower.starts_with("https://") {
        raw.to_owned()
    } else if is_loopback_hostish(&lower) {
        format!("http://{raw}")
    } else {
        // /^[\w.-]+(:\d+)?([/?#]|$)/ — bare host, optional port, then path/end.
        let host = lower.split(['/', '?', '#']).next().unwrap_or("");
        let (host, port_ok) = match host.split_once(':') {
            Some((h, p)) => (h, !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            None => (host, true),
        };
        if !port_ok
            || host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return None;
        }
        format!("https://{raw}")
    };
    // `parsed.protocol` http/https, hostname non-empty, no credentials.
    if !url_shape_ok(&candidate, false) {
        return None;
    }
    Some(candidate)
}

/// `hostOf` — the small label chip at the right of the address bar. file:
/// URLs have no host; the file name is the useful label instead.
pub fn host_of(url: &str) -> String {
    let after_scheme = match url.split_once("://") {
        Some(("file", rest)) | Some(("FILE", rest)) => {
            let last = rest
                .split('/')
                .filter(|s| !s.is_empty())
                .next_back()
                .unwrap_or_default();
            return percent_decode(last);
        }
        Some((_, rest)) => rest,
        None => return String::new(),
    };
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .split('@')
        .next_back()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    host.strip_prefix("www.").unwrap_or(host).to_owned()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// `PROTECTED_CHECKOUT_PATH` — path segments that name a checkout or payment
// step. The JS regex is a segment-anchored alternation; we decompose it into
// per-segment rules, which is equivalent on path text.
fn checkout_segment(segment: &str) -> bool {
    // `(?:\.[a-z]+)?` — a trailing extension like `.php` rides along, so
    // strip the final `.ext` before comparing.
    let lower = segment.to_lowercase();
    let seg = match lower.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().all(|c| c.is_ascii_lowercase()) => {
            stem.to_owned()
        }
        _ => lower,
    };
    let seg = seg.as_str();
    const EXACT: &[&str] = &[
        "checkout",
        "checkouts",
        "check-out",
        "payment",
        "payments",
        "billing",
        "place-order",
        "place_order",
        "complete-order",
        "complete_order",
        "confirm-order",
        "confirm_order",
        "finish-order",
        "finish_order",
        "buy-now",
        "buy_now",
        "purchase",
    ];
    if EXACT.contains(&seg) {
        return true;
    }
    // `checkouts?(?:[-_](now|start|review|payment|confirm(ation)?|complete|finish))?`
    if let Some(rest) = seg
        .strip_prefix("checkouts-")
        .or_else(|| seg.strip_prefix("checkouts_"))
        .or_else(|| seg.strip_prefix("checkout-"))
        .or_else(|| seg.strip_prefix("checkout_"))
    {
        return matches!(
            rest,
            "now" | "start"
                | "review"
                | "payment"
                | "confirm"
                | "confirmation"
                | "complete"
                | "finish"
        );
    }
    // `order[-_](review|confirm(ation)?|complete|finish)`
    if let Some(rest) = seg.strip_prefix("order-").or_else(|| seg.strip_prefix("order_")) {
        return matches!(rest, "review" | "confirm" | "confirmation" | "complete" | "finish");
    }
    false
}

/// Split `scheme://authority/path?query` into (authority, path, query).
/// Scheme-less strings return None — same as `new URL` throwing.
fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    let after = &rest[host_end..];
    let path_end = after.find(['?', '#']).unwrap_or(after.len());
    let path = &after[..path_end];
    let query = after[path_end..]
        .strip_prefix('?')
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("");
    Some((host, path, query))
}

/// `isProtectedCheckoutUrl` — checkout host label, checkout path segment, or
/// a `step|stage|flow|action|page`-ish query key carrying a checkout value.
pub fn is_protected_checkout_url(value: &str) -> bool {
    let Some((authority, path, query)) = split_url(value) else {
        return false;
    };
    let host = authority
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    // `(?:^|\.)(?:checkout|payments?|billing|purchase|orders?)(?:\.|$)` — a
    // hostname label equal to one of those words.
    if host.to_lowercase().split('.').any(|label| {
        matches!(
            label,
            "checkout" | "payment" | "payments" | "billing" | "purchase" | "order" | "orders"
        )
    }) {
        return true;
    }
    if path.split('/').any(checkout_segment) {
        return true;
    }
    // `(key ~ /step|stage|flow|action|page/i) && checkoutPath.test(value)`
    query.split('&').any(|pair| {
        let (key, val) = pair.split_once('=').unwrap_or((pair, ""));
        let key = key.to_lowercase();
        (key.contains("step")
            || key.contains("stage")
            || key.contains("flow")
            || key.contains("action")
            || key.contains("page"))
            && percent_decode(val).split('/').any(checkout_segment)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_url_parity() {
        assert_eq!(to_navigation_url("https://"), None);
        assert_eq!(
            to_navigation_url("example.com"),
            Some("https://example.com".into())
        );
        assert_eq!(
            to_navigation_url("localhost:3000"),
            Some("http://localhost:3000".into())
        );
        assert_eq!(
            to_navigation_url("hello world"),
            Some("https://www.google.com/search?q=hello%20world".into())
        );
        assert_eq!(to_navigation_url("javascript:alert(1)"), None);
        assert_eq!(to_navigation_url("data:text/html,<b>x</b>"), None);
        assert_eq!(
            to_navigation_url("/Users/me/index.html"),
            Some("file:///Users/me/index.html".into())
        );
        assert_eq!(
            to_navigation_url("C:\\Users\\user\\Desktop\\ee\\index.html"),
            Some("file:///C:/Users/user/Desktop/ee/index.html".into())
        );
        assert_eq!(
            to_navigation_url("file:///C:/Users/user/a b.html"),
            Some("file:///C:/Users/user/a%20b.html".into())
        );
    }

    #[test]
    fn checkout_guard() {
        assert!(is_protected_checkout_url("https://shop.example.com/checkout"));
        assert!(is_protected_checkout_url(
            "https://shop.example.com/cart/order-review/"
        ));
        assert!(is_protected_checkout_url("https://payments.stripe.com/pay"));
        assert!(is_protected_checkout_url("https://a.com/x?step=checkout"));
        assert!(is_protected_checkout_url(
            "https://checkout.example.evil.org/"
        ));
        assert!(!is_protected_checkout_url("https://example.com/about"));
        assert!(!is_protected_checkout_url("https://example.com/checkoutting"));
        assert!(!is_protected_checkout_url("not a url"));
    }

    #[test]
    fn strict_control_url() {
        assert_eq!(
            browser_navigation_url("example.com"),
            Some("https://example.com".into())
        );
        assert_eq!(
            browser_navigation_url("localhost:3000"),
            Some("http://localhost:3000".into())
        );
        assert_eq!(browser_navigation_url("hello world"), None);
        assert_eq!(browser_navigation_url("file:///etc/passwd"), None);
        assert_eq!(browser_navigation_url("https://u:p@x.com/"), None);
    }
}
