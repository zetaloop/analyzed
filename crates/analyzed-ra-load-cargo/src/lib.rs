#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![cfg(not(test))]
#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]
#![allow(unfulfilled_lint_expectations)]

include!(concat!(
    env!("OUT_DIR"),
    "/ra_ap_load_cargo_bridge/src/lib.rs"
));
