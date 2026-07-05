#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![allow(unexpected_cfgs)]

#[cfg(not(rust_analyzer))]
include!(concat!(
    env!("OUT_DIR"),
    "/ra_ap_rust_analyzer_bridge/tests/slow-tests/test-support.rs"
));
