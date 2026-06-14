#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![recursion_limit = "128"]
#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]
#![allow(unexpected_cfgs)]
#![allow(unfulfilled_lint_expectations)]

#[cfg(not(rust_analyzer))]
include!(concat!(
    env!("OUT_DIR"),
    "/ra_ap_rust_analyzer_bridge/src/analyzed_root.rs"
));
