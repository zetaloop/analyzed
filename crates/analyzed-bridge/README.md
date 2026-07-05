# analyzed-bridge

Build-time support for the analyzed bridge crates: it locates upstream `ra_ap_*` packages in the local cargo registry, verifies their checksums, unpacks them, and provides the syntax-level editing primitives their build scripts use to patch the source.

Part of [analyzed](https://github.com/zetaloop/analyzed), a shared rust-analyzer daemon.
