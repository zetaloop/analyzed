# <img height="80" alt="analyzed logo" src="./analyzed.svg" />

A shared rust-analyzer daemon.

`analyzed` runs one analysis process for all your editors and projects. The first connection starts a daemon in the background; later sessions attach to it and share the work where they can.

Every rust-analyzer instance loads its own copy of the standard library, dependency crates, and inferred types. Open the same project in two editors, or two projects on the same toolchain: most of that data is identical. `analyzed` keeps it once: sessions that share a compatible toolchain and configuration share the same analysis.

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

Point your editor's rust-analyzer binary path at `analyzed`. It talks LSP on stdio, just like `rust-analyzer`.

Managing the daemon:

```sh
analyzed status              # daemon state, as JSON
analyzed stop                # shut the daemon down
analyzed daemon --foreground # run the daemon in the current terminal
```

## How it works

The daemon runs the upstream rust-analyzer main loop for every session, so each editor connection behaves like a normal rust-analyzer instance: same configuration, same features, same diagnostics.

`analyzed` is built from the published rust-analyzer crates (`ra_ap_*`) with modifications applied at build time. Each release targets one upstream version and passes its test suites, including the LSP end-to-end tests.

## License

MIT. The rust-analyzer code it builds on remains under the upstream MIT/Apache-2.0 dual license.
