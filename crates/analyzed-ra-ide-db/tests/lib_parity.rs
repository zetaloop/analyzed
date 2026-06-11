#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![allow(macro_expanded_macro_exports_accessed_by_absolute_paths)]
#![allow(unexpected_cfgs)]
#![allow(unfulfilled_lint_expectations)]

#[cfg(not(rust_analyzer))]
include!(concat!(env!("OUT_DIR"), "/ra_ap_ide_db_bridge/src/lib.rs"));
