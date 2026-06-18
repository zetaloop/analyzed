# analyzed

A shared rust-analyzer daemon. The daemon runs the upstream rust-analyzer main loop for every connected session; sessions with a compatible toolchain and Cargo configuration attach to the same analysis backend, sharing work instead of duplicating it.

Each release pins one upstream version (the `ra_ap_*` crates) and must pass the upstream test suites. To an editor, `analyzed` must behave like a correct `rust-analyzer` instance: same configuration, same features, same diagnostics. External behavior stays identical to upstream; only the internal sharing differs.

## Architecture

A SharedWorld is the sharing boundary. It holds one AnalysisHost, one RootDatabase, and the loaded workspaces for that world.

- The registry groups sessions by `SharedAnalyzerWorldKey`. Incompatible toolchains or Cargo/load settings get their own world.
- Workspace roots, linked projects, excluded paths, and client analysis settings belong to the view. A session is a view over a world, not another database, and may see only part of the merged crate graph.
- When a workspace loads into a world, its source roots and crate graph merge into the shared database. Crate sharing relies on salsa interning: identical crate inputs re-intern to the same ID, so shared dependencies are analyzed once.
- SharedWorld is the only path to the database. New write paths must go through its input application code instead of touching the host directly, otherwise other sessions see inconsistent state or miss coordination.
- Open files stay on the shared base while their contents match disk. Unsaved changes use session-local overlay file IDs and overlay crate cones. When the buffer converges back to the on-disk text, the overlay is removed and the session returns to the shared path.

Do not add per-test or per-session world separation. The project only proves its value when real sharing works. Type interner GC is process-global: it runs only when no session in any world is busy.

## Patching

The `analyzed-ra*` crates patch unpacked upstream sources in `build.rs`, then compile the result from `OUT_DIR`.

We keep our modifications to the upstream code minimal. Copying large blocks of upstream logic into our own files is the wrong approach. Instead:

- Work at the symbol level where possible: adjust visibility, rename a function, add a field or parameter.
- When a function body must change: rename the upstream function to `_original_name`, write a new function with the original name that delegates to `_original_name` where possible. When delegation isn't feasible, mark `_original_name` as `#[allow(dead_code)]`.
- For logic that's inlined inside a large function and can't be reached otherwise: use `extract_method` to pull the relevant portion into a nested function, then follow the same rename-and-replace approach. Do this only when necessary.
- Logic duplicated from upstream (excluding variable renames and type adjustments) should not exceed roughly 30% of the function. A few-line helper is fine. The threshold is about real duplication, not counting lines.
- Don't alter upstream execution flow or response timing to avoid duplication. The external behavior must stay identical to upstream.

Build script patches are applied in order; earlier replacements affect the text seen by later ones. Don't edit generated files. Fix the patch source and rebuild.

## Platform & IPC

Endpoints:

- Linux: `$XDG_RUNTIME_DIR/analyzed/daemon.sock`
- macOS: `$TMPDIR/analyzed/daemon.sock`
- Windows: `\\.\pipe\analyzed.<USERNAME>`, overlapped I/O

The daemon leaves nothing in the user's home directory. On Unix, the runtime directory holds only the socket.

## Verification

Upstream parity tests must pass under default parallel execution with shared state. This is the core guarantee: running under sharing and concurrency must not produce any inconsistencies compared to standalone rust-analyzer.

- Don't serialize test suites or isolate shared state to make tests pass. If a test only passes with `--test-threads=1`, there's a real sharing bug.
- Static checks (`fmt`, `check`, `clippy`) at zero warnings. No lint bypass comments.
- Don't add new tests for shared behavior unless asked.

## Crate Layout

The workspace has several crates under `crates/`. The ones relevant for most changes:

- `analyzed`: CLI binary: `analyzed status`, `stop`, `daemon`, and the stdio-to-daemon bridge.
- `analyzed-daemon`: service: socket/named-pipe listener, session management, backend lifecycle.
- `analyzed-ipc`: protocol types and transport for the daemon <> client channel.
- `analyzed-bridge`: build-time helper crate: unpacks upstream crates from the local registry, verifies checksums, and provides source-manipulation primitives.

The remaining `analyzed-ra*` crates are the bridge crates that mirror one `ra_ap_*` upstream crate each. Their `build.rs` patches the upstream source and re-exports the result. The crate name tells you which upstream layer it wraps (e.g., `analyzed-ra-ide-db` wraps `ra_ap_ide_db`).

## Workflow

- Don't push, revert, switch branches, or clean the working tree without asking first.
- Finish one coherent change, verify it, commit it. Then start the next. Don't batch unrelated work.
- Prefer minimal fixes. Avoid adding polling, caches, or compatibility branches unless the code proves they're needed.
