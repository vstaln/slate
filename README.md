> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Slate 0.0.1

One native Rust binary (`slate`). Five widgets: **terminal, planner, files, browser, orchestration**. No Electron, no Chromium, no Node in the binary. The single binary serves as both the desktop GUI window and the CLI command harness.

- **Terminal** — `portable-pty` + `vt100`/`vte` engine; snapshots render in rgpui.
- **Browser** — vendored wry element. X11 renders inline in the window; Wayland opens a separate GTK window (accepted for 0.0.1).
- **CLI** — `slate workers`, `slate tell <worker> "message"`, `slate plan`, `slate browser`.

## Build & run

```text
npm run native:build   # release binary: slate
npm run native:start   # release build, then open the Slate window
```

`npm run native:run` / `native:check` are the debug / check equivalents.

## Verify

```text
npm test                # version gate + Rust lib tests
node scripts/smoke.mjs  # spawn the release binary, check /health (needs a display)
```

## Layout

- `native/slate-app` — the app: unified `slate` binary (GUI + CLI)
- `cli/slate` — compat CLI shim for agents in shells
- `scripts/` — `check-version`, `smoke`, `native-cargo` passthrough
- `docs/` — 0.0.1 plan + spec
