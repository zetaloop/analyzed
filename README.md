# analyzed

A shared rust-analyzer daemon.

`analyzed` runs one analysis process for all your editors and projects. Editor
sessions connect to a daemon over a local socket; the daemon hosts
rust-analyzer and shares its analysis database between sessions wherever
sharing is possible.

The point is memory. Every rust-analyzer instance carries its own copy of the
standard library, the dependency crates and the types inferred from them. Open
the same project in two editors, or two projects on the same toolchain, and
most of that data is identical. `analyzed` keeps it once: compatible sessions
share one database, and identical types are interned once for the whole
process.

## Installation

```sh
cargo install analyzed
brew install analyzed    # macOS
scoop install analyzed   # Windows
```

Or from source:

```sh
cargo install --path crates/analyzed
```

## Usage

`analyzed` speaks LSP on stdio, exactly like `rust-analyzer`. Point your
editor's rust-analyzer binary path at `analyzed` and you are done. The first
connection starts the daemon automatically.

Managing the daemon:

```sh
analyzed status              # daemon state, as JSON
analyzed stop                # shut the daemon down
analyzed daemon --foreground # run the daemon in the current terminal
```

## How it works

The daemon runs the upstream rust-analyzer main loop for every session, so
each editor connection behaves like a full rust-analyzer instance: same
configuration, same features, same diagnostics. Underneath, sessions whose
toolchain and configuration are compatible attach to the same shared world, a
single salsa database holding all of their workspaces. Each session sees only
the crates of its own projects, while analysis of shared dependencies is
computed once and reused by everyone. Sessions are isolated only where sharing
would actually conflict.

`analyzed` is built from the published rust-analyzer crates (`ra_ap_*`) with
modifications applied at build time. Each release tracks exactly one upstream
version and keeps its behavior: the upstream test suites, including the LSP
end-to-end tests, run unchanged against the modified code.

## License

MIT. The rust-analyzer code it builds on remains under the upstream
MIT/Apache-2.0 dual license.
