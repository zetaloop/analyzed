#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![cfg(not(test))]

include!(concat!(
    env!("OUT_DIR"),
    "/ra_ap_rust_analyzer_bridge/src/lib.rs"
));
