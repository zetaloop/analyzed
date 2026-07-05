# analyzed

`analyzed` is a drop-in replacement for rust-analyzer that runs the analysis in a shared daemon. The daemon runs the upstream rust-analyzer main loop for every connected session; sessions with a compatible toolchain and Cargo configuration attach to the same analysis backend, sharing work instead of duplicating it.

Each release pins one upstream version (the `ra_ap_*` crates) and must pass the upstream test suites. To an editor, `analyzed` must behave like a correct `rust-analyzer` instance, with the same configuration, features, and diagnostics. External behavior stays identical to upstream; only the internal sharing differs.

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

- Patches are declared in `build.rs` with typed parameters and applied through the syntax editing helpers in `analyzed-bridge`. Anchor on symbols and structure only: no ordinal positions, no statement text, no textual search.
- Work at the symbol level where possible: adjust visibility, rename a function, add a field or parameter.
- When a function body must change: rename the upstream function to `_original_name`, inject a replacement with the original name that delegates to `_original_name` where possible. When delegation isn't feasible, mark `_original_name` as `#[allow(dead_code)]`.
- For logic that's inlined inside a large function and can't be reached otherwise: extract the relevant region into a method, then follow the same rename-and-replace approach. Do this only when necessary.
- Logic duplicated from upstream (excluding variable renames and type adjustments) should not exceed roughly 30% of the function. A few-line helper is fine. The threshold is about real duplication, not counting lines.
- Don't alter upstream execution flow or response timing to avoid duplication. The external behavior must stay identical to upstream.
- Injected names carry no ownership markers. A replacement keeps the upstream name; a new item takes a name in upstream style, checked for collisions; an owned module whose name clashes with an upstream file takes a `shared_` prefix. The patch declarations in `build.rs` are the authoritative list of injection points.

Patches apply in order; an earlier rename changes what later anchors resolve. Don't edit generated files. Fix the patch source and rebuild. When bumping the upstream pin, update the whole `ra_ap_*` family together and let the build derive and verify the version identity; there is no manual version mapping. The release facts (target matrix, runners, build flags, PGO setup, packaging) live in `xtask`'s target table, and the release workflow derives its job matrix from `cargo xtask matrix`. They mirror the upstream release configuration; nothing syncs them to upstream automatically, so reconcile them in the same bump.

## Platform & IPC

Endpoints:

- Linux: `$XDG_RUNTIME_DIR/analyzed/daemon.sock`
- macOS: `$TMPDIR/analyzed/daemon.sock`
- Windows: `\\.\pipe\analyzed.<USERNAME>`, overlapped I/O

The daemon leaves nothing in the user's home directory. On Unix, the runtime directory holds only the socket.

## Verification

Upstream parity tests must pass under default parallel execution with shared state. This is the core guarantee: running under sharing and concurrency must not produce any inconsistencies compared to standalone rust-analyzer.

- The suites are `lib_parity` and `lsp_parity` in `crates/analyzed-ra`. They run only with `RUN_SLOW_TESTS=1`; a 0-second finish means they were skipped. `lsp_parity` must finish within 60 seconds.
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

`xtask` at the workspace root is the release tooling: `cargo xtask dist` builds and packages the release artifact for one target, taking `--training-dir` for PGO targets; `cargo xtask matrix` prints the CI job matrix from the same target table.

## Workflow

- Don't push, revert, switch branches, or clean the working tree without asking first.
- Finish one coherent change, verify it, commit it. Then start the next. Don't batch unrelated work.
- Prefer minimal fixes. Avoid adding polling, caches, or compatibility branches unless the code proves they're needed.
