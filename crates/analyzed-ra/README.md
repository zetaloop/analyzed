# analyzed-ra

The analyzed build of [`ra_ap_rust-analyzer`](https://crates.io/crates/ra_ap_rust-analyzer). The build script unpacks the checksum-verified upstream source from crates.io, patches it at build time, and compiles the result with the shared-analyzer entry points added; the package ships no upstream code.

Part of [analyzed](https://github.com/zetaloop/analyzed), a shared rust-analyzer daemon.
