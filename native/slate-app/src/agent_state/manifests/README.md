# Agent state manifests

Rule sets for `slate_app::agent_state` — the manifest-driven screen-tail
classifier ported from **herdr** (`github.com/herdrdev/herdr`,
`src/detect/`, Apache-2.0). `claude.json`, `codex.json`, and
`opencode.json` are ports of herdr's TOML manifests — same rule ids,
states, and priorities so they stay easy to sync upstream; the rule
grammar (`all`/`any`/`not` over `contains`/`regex`/`line_regex` in named
regions) is a JSON rendering of theirs.

`gray.json` is original to slate, written against Gray's own chrome
(`esc to interrupt` while working, `Enter submit · Esc skip` in its ask
modal, the bare `❯` composer at idle).
