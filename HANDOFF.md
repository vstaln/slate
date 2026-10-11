# Handoff — Slate/OrcSpace parity audit + gray ecosystem state

> Written for the next agent. Everything below was verified against code/git this
> session; nothing is inferred. Read this file before exploring — it maps where
> everything lives and what is already done.

## 1. Repos, worktrees, installed binaries

| Thing | Path | State |
|---|---|---|
| Slate app repo | `/home/vstaln/slate` | `main` @ `238799d`; uncommitted rebrand churn in `AGENTS.md`/`CLAUDE.md`/`GEMINI.md`/`README.md` + `assets/icons/*` (leave or commit separately — not yours) |
| **Original Electron source** | `/tmp/orcspace-orig` | `git worktree` of `f426c42^` (commit before Electron tree was deleted). **This is the parity reference — use it, do not decompile the asar** |
| Packaged original app | `~/.local/opt/OrcSpace/` | Still installed + runnable (Electron, v2.2.x). `app.asar` extracted to `/tmp/orcspace-src` |
| Rust app | `~/slate/native/slate-app` | gpui (`zed` git tag `v1.22.0`, `gpui_platform` x11) |
| Running app | `slate --headless` (release, live) | `~/.cargo/bin/slate` ELF, rebuilt Oct 5 ~20:49 (parity wave 2 + headless + journal flock) |
| Agent CLI source | `~/slate/cli/slate.mjs` (64KB, full port of `orc.mjs`) | Installed shim `cli/slate` execs native bin if present, else node mjs |
| Original CLI ref | `~/.local/opt/OrcSpace/resources/cli/orc.mjs` | 64KB; slate.mjs is a 1:1 port (same command surface) |
| Gray main repo | `/home/vstaln/gray` | clean |
| Gray PR worktree | `/home/vstaln/wt/gray-hangup` (`feat/batch-hangup`) | **merged** via PR #182 (`ef6ca848`) — worktree+branch safe to delete |
| Relay worktree | `/home/vstaln/wt/relay-merge` (`feat/relay-merge`) | PR **#183 OPEN** — per-turn `provider/chat` relay handshake; makes sub plugins actually serve chat |
| Provider-chat worktree | `/home/vstaln/wt/provider-chat` | has uncommitted WIP (SSE event-trail + entity decoder) — **not mine, do not disturb** |
| Plugins dir | `/home/vstaln/grayplugins/` | independent repos, NOT a monorepo |

## 2. Why Slate "is so bad" — the actual diagnosis

Commit `173ea7d` replaced the infinite canvas with a **5-tab strip**
(`shell.rs`: Terminal/Planner/Files/Browser/Orcstration) as a gpui placeholder
after the eframe→rgpui→gpui framework hops. That skeleton is what ships.

- `RootView` (`src/shell.rs`, 202 lines): tab strip, Catppuccin colors
  (`0x181825`/`0xa6adc8`/`0x89b4fa`), `eprintln!` keystroke debug spam, terminal
  input only when tab 0 active, browser hardcoded `example.com`.
- **Dead code already ported**: `projection.rs` (CanvasState: widgets/camera/
  strokes/connections, literal port of `canvasState.ts` incl. journal replay),
  `canvas.rs` (camera math, `drag_target`, `widget_at`, `zoom_at`, `is_visible`
  cull, `CanvasFrame`), `theme.rs` (exact Slate tokens, contract-tested vs
  vendored `tokens.ts`/`WidgetFrame.tsx` fixtures), `engine.rs` (2531-line PTY
  manager), `terminal_screen.rs` (vt100 + key/mouse encode), `journal.rs` +
  `journal_log.rs` (hash-chained `command-journal.ndjson`, byte-compatible).
- TerminalSnapshot gives `screen: String` — **plain text, no per-cell colors**
  (xterm parity needs cell attrs; vt100 exposes them).
- gpui `canvas` element exists for custom paint (connections/strokes).

## 3. Original app structure (from `/tmp/orcspace-orig`)

- 103 renderer files + 187 main files; event-sourced: journal →
  `core/projections.ts` → `commands/*` → `controlServer.ts` (1398 lines, the
  IPC/REST router; Rust `http.rs` ports it but only has `/orchestration/*`).
- `App.tsx` (2043 lines): wallpaper → TitleBar → Sidebar → canvas area
  (world transform, marquee, crosshair guides) → ConnectionsLayer,
  StrokesLayer → ContextMenu → Toolbar (command bar) → StatusBar. Empty-state
  overlay hints `/terminal`, `/chat`, `.files`, `@planner` + keys
  `T N + - 0 F Home`.
- Components: `WidgetFrame` 784, `TerminalWidget` 1695 (xterm), `Sidebar` 1593,
  `CodeView` 1716 (separate Code-workspace mode), `PlannerWidget` 881,
  `FilesWidget` 900, `BrowserWidget` 705 (webview), `OrchestrationWidget` 460,
  `TitleBar` 753, `Toolbar` 313, `ContextMenu`/`StrokesLayer`/`ConnectionsLayer`.
- `lib/commandInput.ts`: first-token prefixes `/ . @` + `WIDGET_ALIASES`
  (term/sh/shell/cmd→terminal; file(s)→files; plan/tasks/todo→planner;
  orc(h)/agents/workers→orchestration; web→browser); rest of line =
  `initialCommand` queued to the spawned terminal (`queueInitialCommand`).
- `types.ts`: `WIDGET_W=680 H=420`; `WIDGET_DEFAULTS`: terminal 680×420,
  planner 420×520, files 580×480, browser 720×480, orchestration 520×560;
  `MIN_W=280 MIN_H=160`; `WIDGET_MAX_SIZE` caps files 760×620, orchestration
  760×720; `clampWidgetSize` clamps size FIRST then derives x/y;
  `NON_MAXIMIZABLE={orchestration}`; `STROKE_COLORS` 8-color palette.
- Placement: `placeWidget` clamps into visible world (32/z margin, min Y below
  title bar); spawn-at-center = `toWorld(center) − half size`.
- `useCanvas.ts` → `canvas.addWidget(point,id,title,kind,media)`; mutations
  persist via journal commands: `widget.create|update|remove`,
  `canvas.camera|connections|strokes|import`.
- Browser popup policy: `window.open` bursts >3/5s suppressed (popup blocker).
- Widget kinds known to projection (must match `WIDGET_KINDS`):
  terminal, timer, planner, files, sys-monitor, browser, image, links,
  music-player, orchestration, chat, notes, calendar, kanban. Unknown kind =
  dropped widget.

## 4. IPC surface of the original (from `out/preload/index.cjs`)

Domains: `window:*`, `browser:*` (agent-action/open-in-code/clear-data),
`terminal:*` (list/create/write/resize/dispose/set-title/set-last-prompt/
detach/ack-output/focus + onData/onExit/onPrompt/onBackendError), `media:*`
(clipboard read/write, save-bytes[-scratch], stage-clipboard-image),
`workspace:*` (get-dir/pick-dir/rename/code-workspaces CRUD/recent/pin/forget/
select-code), `updates:*`, `settings:*` (get/set/background pick/clear),
`chat:*` (providers/connect/send/cancel/auth-submit), `orchestration:*`,
`planner:*`, `notes:*` (recolorCategory), `git:*`, `canvas:*` (load/save/
replay/update-widget + onChange/onDelta), `code:*`, `fs:*`, `system:*`
(cpu/memory/stats/persistError/release-locks), `control:*` (add/remove/
rename-widget, open-media), `renderer-state:*`.

## 5. Slate backend status (Rust, `native/slate-app/src`)

| Piece | Status |
|---|---|
| `http.rs` routes | **only `/orchestration/*`, `/terminal/{id}/output`, `/health`** — canvas, planner, terminal-mgmt, git, files, code, workspace, locks, journal, snapshot, presence, screenshot all missing ("block 4" in `docs/RUST-MIGRATION.md`) |
| `cli.rs::plan()` | orchestration verbs + status/version only; `whoami`/`workers`/`tell`/`worker-read` special-cased in `cli_run.rs`. `slate canvas`/`plan`/`browser`/`terminal`/`git`/`journal`/`rename`/`context`/`doctor`/`api` = unknown |
| `journal_log.rs` | `JournalLog::open(path)` + `commit(actor,type,target,payload)` — works; file: `user_data_dir()/command-journal.ndjson` |
| `engine.rs` | `TerminalManager::spawn/spawn_with_options/snapshots/drain_events/key_input/resize/set_name/resolve` — solid |
| Views | `views_terminal.rs` (text-lines render), `views_planner`, `views_files`, `views_orchestration`, `views_browser` (placeholder div — wry element was dropped in the gpui migration) |

## 6. The plan I was executing (paused for user confirmation)

1. `CanvasView` (gpui) replacing RootView: load CanvasState from journal
   replay, render WidgetFrames at world coords (theme.rs chrome: 34px header,
   hairline, rounded body, active ring), camera transform.
2. Interactions: pan empty drag, wheel zoom-at-pointer (clamp 0.2–4), header
   drag, close/focus, keys T/N/F/Home/+/-/0.
3. Widget dispatch: terminal → vt100 screen; planner/files/orchestration →
   existing views; browser → placeholder.
4. Persistence: `journal.commit` on create/update/remove/camera; respawn
   terminal PTYs on launch.
5. Command bar + empty state.
6. Retheme views off Catppuccin; kill `eprintln!`.
7. Port remaining route domains so CLI verbs work.

## 7. Gray-side state (separate project, for context)

- **PR #182 merged** (`ef6ca848`): batch/hangup durability, `--bare`,
  tool-preview contract, lock-field retention, poll cap, README installer
  fix; all CodeRabbit findings dispositioned (8 fixed, 2 already-correct,
  2 false positives — the `#[cfg(unix)]` signal guard and `setup`-does-
  install one).
- **PR #183 open** (`feat/relay-merge`, commit `da23810e`): merges
  `feat/provider-chat-relay` into new main — **load-bearing**: without it,
  sub providers authenticate but turns POST to dead `127.0.0.1:1` placeholder.
- gray-claude-sub / gray-antigravity-sub / gray-devin-sub: pushed to
  `github.com:vstaln/*.git`.
- gray-discord-plugin: PR #9 merged; local branches `fix/request-limit-…`,
  `fix/send-ui-errors`, `feat/cron-card-v2` all merged to local `main`
  (commits `230b4df`, `728aa6e`, `3b1e12f`) — **not yet pushed**. Its rust CI
  has a pre-existing failure on main (`stream.rs:252`).
- Dead branches safe to drop: `feat/claude-sub-plugin`, `feat/batch-*`,
  `feat/bare-minimal`, `wip/*` snapshots; `stash@{0}` content all committed.

## 8. Environment constraints (hard rules)

- **NEVER `cargo test` while X runs** — amdgpu page-flip storm kills Xorg.
  Use `nice -n 19 ionice -c3 flock /tmp/cargo.lock cargo check` only.
- `CARGO_BUILD_JOBS=4` for interactive builds; pre-commit hooks run
  `cargo test` → `git config core.hooksPath /dev/null` + `--no-verify`
  already set in gray repos.
- `git add -A` forbidden on shared checkouts; commit only own files.
- Don't print secrets: `~/.gray/gateway.yaml`, `auth.json`, tokens.
- Don't disturb `~/wt/provider-chat` uncommitted WIP.
- To view images: `gray view <path>` (never `cat` binaries).
- Kill X-risky stuff: the running slate is `native/target/release/slate --gui`.

## 9. Parity wave status — all six lanes landed, dispatch.start ported

- Full gap audit written to `/tmp/parity-audit.md` (rewriting a copy into the repo would churn with the code; regenerate if stale).
- **All six agent lanes merged and compiled**: CHROME (erase/strokes/colors/header buttons/agent menu), PANES2 (all views_* interactions via `widget_command`), CLI2 (full orc.mjs verb surface), TERM2 (mouse modes, bracketed paste, focus events, kitty flags, numpad, DECRPM/XTVERSION replies), ENGINE2 (route parity + raise mailbox + `take_raise_request`).
- **`dispatch.start` is now the faithful port** (`http.rs`): resolves task+run → validates agent → resolves `terminalId` or spawns a fresh terminal titled `<agent>: <task title>` → refuses terminal already running a dispatch → creates the record → builds+persists the full worker preamble → **launches the agent command, waits 2.5s, sniffs the tail for launch failure** → injects the flattened preamble in 8000-byte chunks (Enter on last chunk only) → sends the dispatch mail. On any post-creation failure the dispatch settles `failed` and the fresh terminal is disposed — same as the shell's catch. `RouteEnv` grew `resolve_terminal`/`spawn_terminal`/`write_terminal`/`tail_terminal`; `EngineEnv` implements them via `TerminalManager`.
- **CLI no longer pre-spawns** a terminal for `worker-start`/`dispatch` — the old `POST /terminal` shim bypassed the route's launch+sniff path entirely; the original CLI (orc.mjs) also lets the server reserve.
- **Second-instance handoff**: a second `slate --gui` POSTs `/raise` on the running socket and exits (the shell's `requestSingleInstanceLock` → `second-instance` → `focusMainWindow`).
- **Verified live**: preamble injection e2e, launch-failure sniff + settle-failed + task-failed + terminal dispose, `--no-inject`, OSC 52 → X clipboard, SGR mouse reporting, command-bar `/term` spawn, maximize↔restore, journal live reload (`canvas place/move` appear without restart), journal replay on restart, `canvas image`/`widgets`/`focus`, `/screenshot` route, raise handoff.
- **Pointer-only paths reviewed-but-not-e2e** (xdotool XTEST input correlates with the gpui X11 deaths below): drag/resize handles, draw/erase strokes, widget-pane button clicks, kitty keys, numpad, DECSET-1004 focus reporting.
- Added this session: `Widget.state: Option<Value>` (journaled per-widget UI state), `CanvasView::widget_command` (ops `set`/`plan_toggle`/`plan_move`), `canvas place/image/widgets` CLI verbs, fixed-size ↗/↙/× header buttons (IBM Plex lacks ⤢/⤡), journal mtime live-reload in the canvas tick.
- Known instability: gpui's X11 client exits on `ConnectionError::IoError` — observed four times, always during synthetic xdotool XTEST input or WM float/resize churn. Upstream fragility (`gpui_linux` x11 client), not app logic; real-input risk appears lower but unproven.
- **Session persistence (the shell's `terminalSnapshots`)**: every live terminal's 64 KiB output tail + OSC-7-tracked cwd + name is written to `~/.config/Slate/terminal-state/<id>.json` every 30s from the canvas tick and once on `on_window_closed`. `spawn_with_options` re-feeds the saved tail through the vt100 parser (old scrollback on screen, fresh prompt beneath) and reuses the saved cwd when the caller gave none. `dispose` deletes the state file so closed terminals don't resurrect.
- **Post-audit fixes**: `kind: None` widgets are terminals (the shell's `isTerminal`); paste coverage Ctrl+V/Ctrl+Shift+V/Alt+V/Shift+Insert (bare Ctrl+V no longer sends `^V`); `scrollOnUserInput`; Shift+PgUp/PgDn/Home/End/arrows scrollback nav (Ctrl×5); `mark_exited` + auto-close widget on `Exited` (the shell's `onProcessExit ?? onClose`); single-instance raise; second-instance handoff.
- **Audit reports in flight** (8 read-only critics, `/tmp/parity-audit.md` is the older baseline): widget panes landed — see terminal-pane P0 "no text selection", planner P0 "no create UI", files P0 "no ops/preview", orchestration P0 "read-only dump", cross-pane P1 "no scrolling; wheel zooms camera".
- REMOVED-BY-DESIGN (not gaps): browser widget (`browser widgets are not supported in the native build`), connections/journal-append CLI verbs (absent from the original command table), music/chat/code-view surface.
- **Parity wave 2 (canvas + routes + workspace)**: wheel pan vs Ctrl-zoom sign, `exp()` zoom curve, widget-header wheel swallow, 3px drag threshold, snap guides (8px/zoom edge+center+origin, Alt frees), marquee select tool (`s`), middle/shift-left pan overrides, frame-focus keys (arrows nudge 1/16px, Alt resize, Del→confirm modal), terminal text selection + Ctrl+Shift+C/X, Shift+scrollback nav, `scrollOnUserInput`, paste coverage, exit marker + auto-close, `APP_OWNED_MODE_RESET` + exit codes, orphan-mode recovery on restore, `{error,code}` envelopes, `agentId` body>query>header precedence, 64KiB write cap, attach defaults/limits, exact-id DELETE, tell cap 8 + `agent` field, `worker_done` settlement mail, server-side inbox/replies long-poll (`wait/timeoutMs`, `waited:true`), `resolveWorker` ladder, `needs_confirm` exit-1 gates, parse camelize/`--no-x=v`/JSON lists, guide managed blocks, `runtime.json`, SIGINT/SIGTERM flush, `worker-release --close`, notes atomic store.
- **Workspace model** (`workspace.rs` + `canvas_store.rs`): `workspace-state.json` holds `workspaceDir` + recents; `ensure()` adopts launch cwd on first run and retags untagged journal entries (field sits outside the hash — chain survives); every commit tags `workspaceDir` (payload field wins, else current); per-workspace canvas = `workspace-canvas-<sha256(dir)[:32]>.json` snapshot + journal tail filtered `entry.workspace_dir == dir`; snapshot flushed every ≥50 events on the 30s tick and once on quit; `/presence` + `/health` + `/workspace/code` report the picked dir; terminal spawn cwd = request > workspace > saved > cwd.
- Iteration: `cargo build -p slate-app` (dev, deps cached in shared build-dir) — release only for installs.
- **Headless mode**: `slate --headless` (or `SLATE_HEADLESS=1`) runs the full app on gpui's `HeadlessClient` — control socket, journal, workspace, PTYs all live, no X11 window. Arg dispatch treats args that are only `--gui`/`--headless` as app mode, so `slate --headless <verb>` still runs the CLI. `--engine` remains the stdin/stdout PTY pump (different thing). Caveat: `slate screenshot` can't capture headless. Verified: PTY spawn + `tell` + `read` round-trip.
- **Journal multi-writer race fixed**: `JournalLog::commit` holds an exclusive `flock` on `<journal>.lock` across reload-tail + append (the TOCTOU hole produced duplicate seq 164 + chain break). Journal repaired; `entries=174 max_seq=174 dup_seqs=[] chain_breaks=0` after a rapid create/close barrage.

## 10. Canvas removed — fixed pane strip shell

- The infinite canvas is gone as the interaction model. `CanvasState` survives
  purely as the journal/projection owner (widgets, workspace, `command:` queue,
  `pending_spawn`); camera zoom is pinned to 1.0 and never mutated by input.
- `CanvasView::render` no longer paints wallpaper/strokes/connections/marquee/
  snap guides. The work area shows ONE widget full-size in a fixed rect
  (`pane_rect`: below a new 30px tab strip, right of the sidebar rail, up to
  CODE_MAX_WIDTH). A `slate-pane-strip` tab row (z-ordered chips, × on the
  active tab) replaces world navigation; Tab / Ctrl+Tab / Ctrl+Shift+Tab cycle,
  numpad +/- / Ctrl+O spawn `shell`/`finder`/`editor` panes.
- Close confirmation (`×` / Delete on a focused frame) now hard-requires the
  `confirm` arg — `dangerouslySkipConfirm` honored.
- Mouse: strokes of gesture machinery deleted; remaining handlers only do
  click-to-show, drag-move (rewrites x/y so journals stay sane), header/corner
  resize, browser/files/planner input, marquee-drop gone. `on_key` lost all
  canvas/draw/select/camera keys; Ctrl+wheel is a no-op; plain wheel scrolls
  the hovered pane.
- `commit_camera`, `fit_canvas`/`f`, `key_up` all removed; camera requests
  resolve to "activate the pane". `set_layout`/`cycle_arrange` still move
  widgets but the strip is the real navigation.
- Titlebar canvas tab → "PANES"; settings canvas section → "PANES: tab strip +
  fixed work area" (gesture bullets removed); sidebar empty state lost the
  "draw on the canvas" hint; command-bar `/f`-fit hint gone from the widget list.
- AGENTS.md (repo root + `.claude/CLAUDE.md` + `.agents/AGENTS.md` mirrors)
  rewritten to describe the pane-strip app; native AGENTS.md updated.
- CLI/journal surface unchanged — `slate canvas *`, `/canvas/*` routes still
  work against the journaled store (compat layer), they just no longer drive a
  visible infinite world.
- Verified: `cargo check` clean; full `cargo test` green (incl. http_routes
  agentId fixtures, WriteRequest Option<Value> asserts, file-URL space
  encoding, transport `x-agent-id`).
