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
    // `new URL(raw).href` percent-encodes unsafe bytes — whitespace
    // included — rather than refusing them.
    let rest = &raw[5..];
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

// --- agent actions -----------------------------------------------------------
//
// Port of `shared/browserActionScripts.ts` + the `BrowserAgentAction` union the
// renderer dispatched over `orcspace:browser-agent-action`. The scripts below
// are verbatim transcriptions — only the TS `${...}` interpolations became
// `__PLACEHOLDER__` tokens, filled by the builders. `JSON.stringify(x)` maps to
// `serde_json::to_string`, which produces the same quoted/escaped JS literal.
//
// wry's WebKitGTK `run_javascript` swallows exceptions (an errored eval answers
// an empty string — see `eval()` in wry's webkitgtk backend), where Electron's
// `executeJavaScript` rejected with the thrown message. To keep the original's
// error text (`checkout and purchase actions are disabled`, `ref is not a text
// field`, …) the builders wrap every script in a try/catch that answers
// `{ ok: true, result }` or `{ ok: false, error }` — the same envelope
// `browser:respond` carried.

/// `BrowserAgentAction` — the `{kind, …}` union `controlServer.ts` validated
/// and `BrowserWidget` dispatched. `ref_index` is the 1-based control index
/// from the latest snapshot (`assertRefInRange` caps it at 100).
#[derive(Debug, Clone)]
pub enum BrowserAgentAction {
    Navigate { url: String },
    Snapshot,
    Click { ref_index: u32 },
    Select { ref_index: u32, value: String },
    Fill { ref_index: u32, value: String },
    Press { key: String, ref_index: Option<u32> },
    Scroll { pixels: f64 },
}

/// `SUPPORTED_PRESS_KEYS` — the keys `press` accepts, validated by the route
/// before an action is ever built.
pub const SUPPORTED_PRESS_KEYS: &[&str] = &[
    "Enter",
    "Tab",
    "Escape",
    "ArrowDown",
    "ArrowUp",
    "ArrowLeft",
    "ArrowRight",
    " ",
];

// The protected-surface regexes, verbatim from browserActionScripts.ts. The
// scripts embed them as `new RegExp(<json-string>, 'i')`, exactly like the
// original's `JSON.stringify(REGEX)` interpolation.
const PROTECTED_ORDER_WORDS: &str = r"\b(?:check\s?out|place\s+(?:an?\s+)?order|submit\s+order|complete\s+order|confirm\s+order|finish\s+order|buy\s+now|purchase|pay(?:ment)?\s+now|confirm\s+payment|order\s+now|(?:continue|proceed)\s+to\s+(?:checkout|payment))\b";
const PROTECTED_CHECKOUT_PATH: &str = r"(?:^|/)(?:checkouts?(?:[-_](?:now|start|review|payment|confirm(?:ation)?|complete|finish))?|check-out|payments?|billing|place[-_]order|order[-_](?:review|confirm(?:ation)?|complete|finish)|complete[-_]order|confirm[-_]order|finish[-_]order|buy[-_]now|purchase)(?:\.[a-z]+)?(?:/|$)";
const PROTECTED_PAYMENT_FIELDS: &str = r"password|passwd|secret|token|credit.?card|card.?holder|card.?name|card.?number|card.?exp|expir(?:y|ation)|valid.?through|cc[-_ ]?(?:number|name|exp|csc|cvv|cvc)|payment.?method|one-time-code|\botp\b|\bcvv\b|\bcvc\b|security.?code|iban|routing.?number|ssn";
const PROTECTED_PAYMENT_FORM_FIELDS: &str = r"password|passwd|secret|credit.?card|card.?holder|card.?name|card.?number|card.?exp|expir(?:y|ation)|valid.?through|cc[-_ ]?(?:number|name|exp|csc|cvv|cvc)|payment.?(?:token|method)|one-time-code|\botp\b|\bcvv\b|\bcvc\b|security.?code|iban|routing.?number|ssn";
const SAFE_CHECKOUT_EXIT: &str = r"\b(?:back|return)\s+to\s+(?:cart|bag)|continue\s+shopping|edit\s+(?:cart|bag)\b";

/// `JSON.stringify` for a script embed — a double-quoted, JS-escaped literal.
fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// `snapshotScript()` — visible interactive controls, ref-numbered in query
/// order (the same indexing click/fill/select address).
const SNAPSHOT_SCRIPT: &str = r#"(() => {
    const visible = (el) => {
      const r = el.getBoundingClientRect()
      const s = getComputedStyle(el)
      return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'
    }
    const nodes = [...document.querySelectorAll('a[href],button,input:not([type="hidden"]),textarea,select,[role="button"],[contenteditable="true"]')]
      .filter(visible).slice(0, 100)
    const controls = nodes.map((el, i) => {
      const tag = el.tagName.toLowerCase()
      const label = el.getAttribute('aria-label') || el.closest('label')?.innerText || el.innerText || el.getAttribute('title') || el.getAttribute('placeholder') || el.getAttribute('name') || ''
      let href = ''
      if (tag === 'a') {
        try { const u = new URL(el.href); href = u.origin + u.pathname } catch {}
      }
      const choices = tag === 'select' ? [...el.options].slice(0, 30).map((option) => ({ label: option.label, value: option.value })) : undefined
      return {
        ref: String(i + 1), tag,
        type: el.getAttribute('type') || '',
        label: String(label).replace(/\s+/g, ' ').trim().slice(0, 160),
        placeholder: el.getAttribute('placeholder') || '',
        href, choices
      }
    })
    let pageUrl = location.origin + location.pathname
    return { title: document.title, url: pageUrl, text: (document.body?.innerText || '').slice(0, 6000), controls }
  })()"#;

/// `clickScript(ref)` — the guarded click: protected order words, payment
/// fields and checkout destinations all refuse before `el.click()` runs.
const CLICK_SCRIPT: &str = r#"(() => {
    const visible = (el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none' }
    const nodes = [...document.querySelectorAll('a[href],button,input:not([type="hidden"]),textarea,select,[role="button"],[contenteditable="true"]')].filter(visible).slice(0, 100)
    const el = nodes[__REFIDX__]
    if (!el) throw new Error('page element no longer exists; take a new snapshot')
    const label = [el.innerText, el.getAttribute('aria-label'), el.getAttribute('title'), el.getAttribute('value')].filter(Boolean).join(' ').replace(/\s+/g, ' ').trim()
    const form = el.form
    const submitText = form ? [...form.querySelectorAll('button:not([type]),button[type="submit"],input[type="submit"]')].map((item) => item.innerText || item.value || item.getAttribute('aria-label') || '').join(' ') : ''
    const formFields = form ? [...form.querySelectorAll('input,select,textarea')].map((item) => [item.type, item.name, item.id, item.autocomplete, item.getAttribute('aria-label'), item.getAttribute('placeholder'), [...(item.labels || [])].map((label) => label.innerText).join(' ')].join(' ')).join(' ') : ''
    const protectedWords = new RegExp(__ORDER_WORDS__, 'i')
    const protectedFields = new RegExp(__PAYMENT_FORM_FIELDS__, 'i')
    const checkoutPath = new RegExp(__CHECKOUT_PATH__, 'i')
    const safeExit = new RegExp(__SAFE_EXIT__, 'i')
    const isCheckoutUrl = (raw) => {
      const candidate = new URL(raw, location.href)
      const checkoutHost = /(?:^|\.)(?:checkout|payments?|billing|purchase|orders?)(?:\.|$)/i.test(candidate.hostname)
      const checkoutStep = [...candidate.searchParams].some(([key, value]) => /step|stage|flow|action|page/i.test(key) && checkoutPath.test(value))
      return checkoutPath.test(candidate.pathname) || checkoutHost || checkoutStep
    }
    const destination = el instanceof HTMLAnchorElement ? el.href : (el.getAttribute('formaction') || form?.action || location.href)
    const onCheckoutPage = isCheckoutUrl(location.href)
    const checkoutDestination = isCheckoutUrl(destination)
    const leavesCheckoutSafely = safeExit.test(label) && !checkoutDestination && (el instanceof HTMLAnchorElement || !form)
    if (protectedWords.test(label + ' ' + submitText) || protectedFields.test(formFields) || checkoutDestination || (onCheckoutPage && !leavesCheckoutSafely)) throw new Error('checkout and purchase actions are disabled')
    el.click()
    return { clicked: label.slice(0, 160), tag: el.tagName.toLowerCase() }
  })()"#;

/// `selectScript(ref, value)` — a `<select>` option by value or label.
const SELECT_SCRIPT: &str = r#"(() => {
    const visible = (el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none' }
    const nodes = [...document.querySelectorAll('a[href],button,input:not([type="hidden"]),textarea,select,[role="button"],[contenteditable="true"]')].filter(visible).slice(0, 100)
    const el = nodes[__REFIDX__]
    if (!el || el.tagName !== 'SELECT') throw new Error('ref is not a select field; take a new snapshot')
    const identity = [el.name, el.id, el.autocomplete, el.getAttribute('aria-label'), el.getAttribute('placeholder'), [...(el.labels || [])].map((label) => label.innerText).join(' ')].join(' ')
    if (new RegExp(__PAYMENT_FIELDS__, 'i').test(identity)) throw new Error('password and payment fields are disabled')
    const wanted = __VALUE__
    const option = [...el.options].find((item) => item.value === wanted) || [...el.options].find((item) => item.label.trim().toLowerCase() === wanted.trim().toLowerCase())
    if (!option) throw new Error('option not found; take a new snapshot and use one of its listed choices')
    el.value = option.value
    el.dispatchEvent(new Event('input', { bubbles: true }))
    el.dispatchEvent(new Event('change', { bubbles: true }))
    return { selected: option.label, value: option.value }
  })()"#;

/// `fillScript(ref, value)` — the native setter + input/change events, so
/// controlled inputs (React et al.) observe the write.
const FILL_SCRIPT: &str = r#"(() => {
    const visible = (el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none' }
    const nodes = [...document.querySelectorAll('a[href],button,input:not([type="hidden"]),textarea,select,[role="button"],[contenteditable="true"]')].filter(visible).slice(0, 100)
    const el = nodes[__REFIDX__]
    if (!el || !['INPUT', 'TEXTAREA'].includes(el.tagName)) throw new Error('ref is not a text field; take a new snapshot')
    const identity = [el.type, el.name, el.id, el.autocomplete, el.getAttribute('aria-label'), el.placeholder, [...(el.labels || [])].map((label) => label.innerText).join(' ')].join(' ')
    if (el.type === 'password' || new RegExp(__PAYMENT_FIELDS__, 'i').test(identity)) throw new Error('password and payment fields are disabled')
    const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, __VALUE__)
    el.dispatchEvent(new Event('input', { bubbles: true }))
    el.dispatchEvent(new Event('change', { bubbles: true }))
    el.focus()
    return { filled: true, ref: '__REFNUM__' }
  })()"#;

/// `pressFocusScript(ref)` — the focus half of a `press` carrying a ref.
const PRESS_FOCUS_SCRIPT: &str = r#"(() => {
    const visible = (el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none' }
    const nodes = [...document.querySelectorAll('a[href],button,input:not([type="hidden"]),textarea,select,[role="button"],[contenteditable="true"]')].filter(visible).slice(0, 100)
    if (!nodes[__REFIDX__]) throw new Error('page element no longer exists; take a new snapshot')
    nodes[__REFIDX__].focus()
  })()"#;

/// `pressKeyScript(key)` — synthetic keydown/keyup on the focused element,
/// with the same Enter+checkout guard the click path applies.
const PRESS_KEY_SCRIPT: &str = r#"(() => {
    const key = __KEY__
    const target = document.activeElement || document.body
    const form = target instanceof HTMLInputElement || target instanceof HTMLButtonElement ? target.form : null
    const label = [target.innerText, target.getAttribute?.('aria-label'), target.getAttribute?.('title'), target.value].filter(Boolean).join(' ')
    if (key === 'Enter') {
      const fields = form ? [...form.querySelectorAll('input,select,textarea')].map((el) => [el.type, el.name, el.id, el.autocomplete, el.getAttribute('aria-label'), el.getAttribute('placeholder'), [...(el.labels || [])].map((label) => label.innerText).join(' ')].join(' ')).join(' ') : ''
      const buttons = form ? [...form.querySelectorAll('button:not([type]),button[type="submit"],input[type="submit"]')].map((el) => el.innerText || el.value || el.getAttribute('aria-label') || '').join(' ') : ''
      const protectedWords = new RegExp(__ORDER_WORDS__, 'i')
      const protectedFields = new RegExp(__PAYMENT_FORM_FIELDS__, 'i')
      const checkoutPath = new RegExp(__CHECKOUT_PATH__, 'i')
      const safeExit = new RegExp(__SAFE_EXIT__, 'i')
      const isCheckoutUrl = (raw) => {
        const candidate = new URL(raw, location.href)
        const checkoutHost = /(?:^|\.)(?:checkout|payments?|billing|purchase|orders?)(?:\.|$)/i.test(candidate.hostname)
        const checkoutStep = [...candidate.searchParams].some(([key, value]) => /step|stage|flow|action|page/i.test(key) && checkoutPath.test(value))
        return checkoutPath.test(candidate.pathname) || checkoutHost || checkoutStep
      }
      const destination = target instanceof HTMLAnchorElement ? target.href : (target.getAttribute?.('formaction') || form?.action || location.href)
      const onCheckoutPage = isCheckoutUrl(location.href)
      const checkoutDestination = isCheckoutUrl(destination)
      const leavesCheckoutSafely = safeExit.test(label) && !checkoutDestination && (target instanceof HTMLAnchorElement || !form)
      if (protectedWords.test(label + ' ' + buttons) || protectedFields.test(fields) || checkoutDestination || (onCheckoutPage && !leavesCheckoutSafely)) throw new Error('checkout and purchase actions are disabled')
    }
    target.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }))
    target.dispatchEvent(new KeyboardEvent('keyup', { key, bubbles: true }))
    if (key === 'Enter' && target instanceof HTMLInputElement && target.form) target.form.requestSubmit()
    else if (key === 'Enter' && target instanceof HTMLButtonElement) target.click()
    return { key }
  })()"#;

/// `scrollScript(pixels)` — instant scroll by the clamped delta.
const SCROLL_SCRIPT: &str =
    r#"(() => { window.scrollBy({ top: __PIXELS__, behavior: 'instant' }); return { scrolled: __PIXELS__ } })()"#;

/// Fill the shared placeholders: the ref index and the protected regexes.
fn fill_guard_placeholders(script: &str, ref_index: u32) -> String {
    script
        .replace("__REFIDX__", &(ref_index - 1).to_string())
        .replace("__REFNUM__", &ref_index.to_string())
        .replace("__ORDER_WORDS__", &js_string(PROTECTED_ORDER_WORDS))
        .replace("__CHECKOUT_PATH__", &js_string(PROTECTED_CHECKOUT_PATH))
        .replace("__PAYMENT_FIELDS__", &js_string(PROTECTED_PAYMENT_FIELDS))
        .replace(
            "__PAYMENT_FORM_FIELDS__",
            &js_string(PROTECTED_PAYMENT_FORM_FIELDS),
        )
        .replace("__SAFE_EXIT__", &js_string(SAFE_CHECKOUT_EXIT))
}

/// The try/catch envelope wry needs for the original's error text — a thrown
/// `Error` in the page becomes `{ ok: false, error: <message> }` instead of an
/// empty eval result.
fn wrap_script(inner: &str) -> String {
    format!(
        "(() => {{ try {{ const result = ({inner}); return {{ ok: true, result }}; }} catch (e) {{ return {{ ok: false, error: e && e.message ? String(e.message) : String(e) }}; }} }})()"
    )
}

impl BrowserAgentAction {
    /// The in-page script for an action, or `None` for `navigate` (handled by
    /// `loadURL`, not JavaScript). Mirrors the kind dispatch inside
    /// `BrowserWidget`'s `onAgentAction`, including `press` running the focus
    /// script before the key script.
    pub fn eval_script(&self) -> Option<String> {
        let inner = match self {
            Self::Navigate { .. } => return None,
            Self::Snapshot => SNAPSHOT_SCRIPT.to_owned(),
            Self::Click { ref_index } => fill_guard_placeholders(CLICK_SCRIPT, *ref_index),
            Self::Select { ref_index, value } => {
                fill_guard_placeholders(SELECT_SCRIPT, *ref_index)
                    .replace("__VALUE__", &js_string(value))
            }
            Self::Fill { ref_index, value } => {
                fill_guard_placeholders(FILL_SCRIPT, *ref_index)
                    .replace("__VALUE__", &js_string(value))
            }
            Self::Press { key, ref_index } => {
                let key_script = fill_guard_placeholders(PRESS_KEY_SCRIPT, 1)
                    .replace("__KEY__", &js_string(key));
                match ref_index {
                    // `pressFocusScript` ran (and could throw) before
                    // `pressKeyScript` — the comma chain keeps that order.
                    Some(ref_index) => format!(
                        "(() => {{ ({}); return ({}); }})()",
                        fill_guard_placeholders(PRESS_FOCUS_SCRIPT, *ref_index),
                        key_script
                    ),
                    None => key_script,
                }
            }
            Self::Scroll { pixels } => {
                let pixels = format!("{pixels}");
                SCROLL_SCRIPT.replace("__PIXELS__", &pixels)
            }
        };
        Some(wrap_script(&inner))
    }

    /// The settle delay the original awaited before answering — click waited
    /// 350ms so a click that navigates lands before the caller's snapshot.
    pub fn settle_ms(&self) -> u64 {
        match self {
            Self::Click { .. } => 350,
            _ => 0,
        }
    }
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
