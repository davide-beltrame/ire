# Workspace

Covers the workspace lifecycle (open, init, close) and the concurrency model that keeps it safe.

---

## Lifecycle

### Onboarding (first launch / no recent workspace)

```
┌─ Setup screen ───────────────────────────────────────┐
│  "Open or create a workspace."                       │
│                                                      │
│  Recent workspaces (up to 5)                         │
│    • each entry shows project name + full path       │
│    • click any entry to open without a file dialog   │
│    • hover an entry to reveal a remove button        │
│    • most-recently-opened is highlighted             │
│                                                      │
│  [Open folder…]       [New workspace…]               │
│                                                      │
│  ● claude-code · ready (or: logged out / not found)  │
│  ● codex · ready       (or: logged out / not found)  │
│    retry button if a binary isn't ready              │
└──────────────────────────────────────────────────────┘
```

On startup, `App.tsx` calls `setup_status` and `read_user_config` in parallel. `read_user_config` removes recent workspace paths that no longer exist, persists the cleaned config, and hydrates `recentWorkspaces` in the Zustand store before the setup screen mounts so the list is immediately populated. `setup_status` reports each binary's `BinaryStatus` as `ready`, `logged_out`, or `missing` — `ready` requires both that the binary was discovered on disk (`find_claude_binary`/`find_codex_binary`) and that a bounded (5s) login-status check succeeds (`claude auth status --json`'s `loggedIn` field, or `codex login status`'s exit code); any failure to confirm login is treated as not ready. If neither binary is `ready`, a `retry` link re-invokes `refreshSetup`; there is no step-by-step wizard. Workspace open/create is enabled when at least one of Claude Code or Codex is `ready`. The `ready` binaries at workspace open/init become the workspace session's initial `availableProviders`; afterward `Layout.tsx` keeps `availableProviders` in sync with the polled `get_system_metrics` result (every 5s), so a login/logout of either CLI while the app is open updates the model picker without a restart.

### Open existing

1. User picks directory via Tauri's file dialog.
2. Backend validates: directory exists, is a git repo, and contains `.ire/_SYSTEM.md` plus `.ire/ire.json` (the marker files).
3. Resolve and create the per-workspace home data dir `~/.ire/workspaces/<name>-<8-hex>/` (`workspace::init::home_data_dir`), then acquire `<data_dir>/.lock`:
   - If absent: write current PID, continue.
   - If present and PID alive: refuse, show "already open in another window".
   - If present and PID dead: reclaim (overwrite with current PID).
4. Initialise SQLite at `<data_dir>/local.db` (`CREATE TABLE IF NOT EXISTS`; no versioned migrations — greenfield).
5. The frontend loads UI/session state via `tauri-plugin-store` (keyed by workspace path) after the open command returns — restores pane layout, open-tab UI metadata, and chat options. Tab messages are not stored there — they are hydrated from the `chat_sessions` table by each tab's `historySessionUuid`.
6. Spawn the MCP server subprocess bound to `<data_dir>/mcp.sock` and write `<data_dir>/mcp.json` (long-lived, lives as long as the workspace is open).
7. Emit `workspace-ready` event to the frontend.

### Initialize new

1. User picks an empty directory (or one without `.ire/`).
2. Backend:
   - `git init` if no `.git/`.
   - Scaffold `.ire/` per the directory layout in [overview.md](overview.md#directory-layout).
   - Create `.ire/{resources,short-term,cache}` and write seed files: `.ire/_SYSTEM.md` (canned framework context), `.ire/ire.json` (seed notes/focus/ideas/experiments), `.ire/long-term.md`, and an empty `resources/_index.md`.
   - Append IRE entries to `.gitignore` (create if missing).
   - Do not stage or commit; the user decides when to commit the initialized workspace.
3. Continue from step 3 of Open existing above.

### Close

- Stop the MCP server (drops `McpHandle`, which aborts the task and removes the socket file).
- Stop the `OpenCodeRuntime` (`opencode::runtime`), if one was ever started this session: abort every OpenCode session it knows about via `POST /session/:id/abort`, then kill the `opencode serve` process. A no-op if no OpenCode turn has run since the workspace opened — the server starts lazily on first use, not eagerly on open (see [chat-agents.md](chat-agents.md#opencode-server-transport)).
- SIGTERM every in-flight CC/Codex subprocess tracked by `SessionManager` and clear all per-tab session state. The frontend `chat-stream` listener is global, so leaving stragglers running would leak late `TextDelta`/`Done` events into whichever workspace opens next.
- Frontend resets the `useChat` Zustand store (`tabs = [MAIN_TAB with empty messages]`, `activeTabId = "main"`) so the next workspace starts with a clean chat pane.
- Release `<data_dir>/.lock` (drops `WorkspaceHandle`, which releases the lock).

---

## Concurrency & Data Safety

Following the decision to **not** adopt the heavy thread-safety blueprint, the model is:

1. **Single-instance per workspace** via the `<data_dir>/.lock` PID file (`~/.ire/workspaces/<id>/.lock`).
   - Created with `OpenOptions::write().create_new(true)` (atomic).
   - Stale detection: parse PID; if not alive (`kill -0` / `OpenProcess`), reclaim.
   - Released on graceful shutdown; orphan-safe via stale reclaim.
2. **In-process serialisation** of `ire.json` writes via `std::sync::Mutex<()>` (`IRE_LOCK`) held by `IreStore`.
3. **Atomic file replacement** for every wiki mutation: temp file in same dir → `fs::rename`. `sync_all` on the temp file before rename.
4. **Agent turn serialisation per session**: one outstanding agent subprocess per session id; new sends queue.
5. **Experiment subprocesses** are detached with their own process group; they outlive an agent subprocess crash.
6. **Checkpoint reconciliation** of changes made to `.ire/` outside the app — a direct file edit, a `git checkout`, or an agent reaching for its built-in `Write`/`Edit` tools instead of `IreStore`. `ire::reconcile(app)` (`src-tauri/src/ire/reconcile.rs`) re-reads `.ire/` and emits the same `workspace-event`s a mutation would, at two points where IRE's own writes have already settled:
   - after each completed tool call (`StreamEvent::ToolDone`, in every agent turn loop: chat, experiment wake-up, resource ingestion, OpenCode);
   - when the main window regains focus (`WindowEvent::Focused(true)`, wired in `lib.rs`), covering edits made while IRE was in the background.

   Running only between IRE's writes, never alongside them, is what lets it skip any mechanism for telling app-originated changes from external ones.

   - **Scanned:** `ire.json` and `resources/*.md` — the two things the frontend mirrors in memory. `_SYSTEM.md`, `long-term.md` and `short-term/` are re-read from disk on every agent turn, and `cache/` is gitignored churn, so none of them can drift.
   - **Two gates:** per-file `(mtime, size)` from the previous pass decides whether a file is read at all; the content hash of what was read decides whether an event is emitted. A pass with nothing new costs one `stat` of `ire.json`, one `read_dir` of `resources/`, and one `stat` per resource — no reads, no hashing. The hash gate is what keeps a `git checkout` quiet when it rewrites mtimes over identical bytes.
   - **State:** the per-file metadata lives in `WorkspaceHandle::ire_snapshot`, so it is created and dropped with the workspace. It is primed at open from the same files the hydrate burst reads, so the first checkpoint reports nothing.
   - **Fails closed:** an `ire.json` that is missing or doesn't parse is left un-recorded and reported as no change, so the panels keep the last state that parsed and the next pass retries. An unreadable `resources/` is not read as "every resource was deleted".
   - **Experiments are excluded.** Their rows carry live tab linkage owned by the runner ([experiments.md](experiments.md)); re-emitting the git-tracked copy from `ire.json` would drop it. An external edit to the `experiments` array is picked up on the next workspace open.
   - **Read-only.** Reconciliation never writes, so `resources/_index.md` still regenerates on `IreStore` writes only: a resource added externally reaches the panel at the next checkpoint, but the agent's catalog at the next resource write.

   A change made mid-turn surfaces at the next completed tool call or focus change, not the instant it lands.

What we explicitly **do not** do (vs. the vault blueprint): file-level advisory lock for the cache, fingerprint CAS, rename WAL with crash recovery, filesystem watcher with noise filtering — external changes arrive through the checkpoint reconcile above, which runs between IRE's own writes rather than concurrently with them and so never has to filter its own noise. If we ever need them (e.g. to support multi-window per workspace), `docs/blueprints/vault-thread-safety.md` is a ready reference.
