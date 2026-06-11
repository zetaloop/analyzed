#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![allow(unexpected_cfgs)]

#[cfg(not(rust_analyzer))]
include!(env!("ANALYZED_RA_SLOW_TESTS"));
