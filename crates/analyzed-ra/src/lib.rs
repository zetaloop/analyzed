#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![cfg(not(test))]
#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]
#![allow(unfulfilled_lint_expectations)]

extern crate self as ra_ap_rust_analyzer;

include!(concat!(
    env!("OUT_DIR"),
    "/ra_ap_rust_analyzer_bridge/src/root.rs"
));
