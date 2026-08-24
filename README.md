# <img height="80" alt="analyzed logo" src="https://raw.githubusercontent.com/zetaloop/analyzed/HEAD/analyzed.svg" />

`analyzed` is a drop-in replacement for [rust-analyzer](https://github.com/rust-lang/rust-analyzer) that runs analysis through a shared daemon.

A normal rust-analyzer process loads the standard library, dependency crates, and inferred types for each editor session. Much of that work is identical across projects on the same toolchain. `analyzed` keeps the shared state in one daemon, so compatible sessions can reuse the same loaded crates and analysis data.

## Installation

```sh
cargo binstall analyzed
```

> We plan to add official packages for [scoop](https://github.com/ScoopInstaller/Scoop/wiki/Criteria-for-including-apps-in-the-main-bucket), [homebrew](https://docs.brew.sh/Acceptable-Formulae), and [nixpkgs](https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md).

Or from a checkout:

```sh
cargo install --path crates/analyzed
```

## Usage

Point your editor's rust-analyzer path at `analyzed`. It uses LSP over stdio. The first connection starts the daemon in the background.

```sh
analyzed status              # daemon state, as JSON
analyzed stop
analyzed daemon --foreground # run in the current terminal
```

## Sharing

Sessions share a backend when their toolchain and Cargo configuration match. Each session keeps its own workspace view, so different roots and nested projects can use the same shared world. Unsaved edits stay private to the session that made them. Each session runs the upstream rust-analyzer main loop. Compatible proc-macro server pools are shared across analysis worlds as well.

## How it works

Before compiling, the build takes the `ra_ap_*` packages selected by Cargo, applies the project patches, and emits the bridge crates used by `analyzed`. Each release targets one upstream version and passes the upstream test suites, including the LSP end-to-end tests.

## License

MIT OR Apache-2.0, same as rust-analyzer.
