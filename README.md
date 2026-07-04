# <img height="80" alt="analyzed logo" src="./analyzed.svg" />

`analyzed` is a drop-in replacement for [rust-analyzer](https://github.com/rust-lang/rust-analyzer) that runs the analysis in a shared daemon.

Most of what an instance loads (standard library, dependencies, inferred types) is identical across editors and projects on the same toolchain; the daemon keeps one copy. A second editor on an already-indexed workspace doesn't re-index.

## Installation

```sh
brew install analyzed     # macOS
scoop install analyzed    # Windows
cargo binstall analyzed
```

Or from a checkout:

```sh
cargo install --path crates/analyzed
```

## Usage

Point your editor's rust-analyzer path at `analyzed`. It serves LSP over stdio, same as rust-analyzer. The first connection starts the daemon in the background.

```sh
analyzed status              # daemon state, as JSON
analyzed stop
analyzed daemon --foreground # run in the current terminal
```

## Sharing

Sessions share a backend only when their toolchain and Cargo configuration match; anything else gets its own. Unsaved edits stay private; other sessions see the on-disk version until the buffer is saved. Every session runs the upstream rust-analyzer main loop and behaves like a normal rust-analyzer instance.

## How it works

The build fetches the checksum-verified `ra_ap_*` sources from crates.io and patches them with structural edits; there is no fork. Each release targets one upstream version, reports it, and passes the upstream test suites, including the LSP end-to-end tests.

## License

MIT OR Apache-2.0, same as rust-analyzer.
