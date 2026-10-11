# Attribution

Slate ports ideas and, in places, implementation grammar from these
projects. Each ported file carries its own provenance comment; this file
collects them.

- **herdr** — `github.com/herdrdev/herdr`, Apache-2.0. The
  agent-state detector (`native/slate-app/src/agent_state.rs`,
  `src/agent_state/manifests/`) is a port of its manifest-driven
  screen-tail classifier (`src/detect/`): same rule grammar, rule ids,
  states (`working`/`blocked`/`idle`/`unknown`), and priorities. The
  `/orchestration/agents` route and `slate agents`/`slate wait` CLI map
  its `agent list`/wait socket surface.
- **T3 Code** — `github.com/pingdotgg/t3code`, MIT. The provider model
  (one built-in harness + user-added `/agent` CLIs persisted as
  `UiPreferences::custom_agents`), pinned/settled thread states, and the
  per-session `git worktree` launch option adapt its product concepts.
- **Orcspace** — the original shell whose layout, launcher, and command
  grammar the canvas views port (per-file headers keep the mapping).
