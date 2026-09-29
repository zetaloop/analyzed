use analyzed_bridge as build_support;
use r#override::{Edition, Source, item, root};

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const PACKAGE: &str = "ra_ap_ide_db";
const GENERATED_DIR: &str = "ra_ap_ide_db_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR, &[], &[])?;
    patch_ide_db_source(&generated.join("src/lib.rs"))?;
    patch_search_source(&generated.join("src/search.rs"))?;
    patch_symbol_index_source(&generated.join("src/symbol_index.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_db_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = Source::parse(&fs::read_to_string(lib_rs)?, Edition::CURRENT)?;
    let visibility = owned_source_path("visibility.rs");
    source
        .select(root())?
        .mount_module("mod visibility", &visibility)?;
    println!("cargo:rerun-if-changed={}", visibility.display());
    source
        .select(item("RootDatabase"))?
        .add_field("visible_files: Option<std::sync::Arc<rustc_hash::FxHashSet<vfs::FileId>>>")?;
    source
        .select(item("RootDatabase::clone").record("Self"))?
        .add_field("visible_files: self.visible_files.clone()")?;
    source
        .select(item("RootDatabase::new").record("RootDatabase"))?
        .add_field("visible_files: None")?;
    fs::write(lib_rs, source.to_string())?;
    Ok(())
}

fn patch_search_source(search_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = Source::parse(&fs::read_to_string(search_rs)?, Edition::CURRENT)?;
    source
        .select(root().import("all_crates"))?
        .redirect("crate::visibility::all_crates")?;
    source
        .select(root())?
        .add_use("use crate::visibility::CrateVisibility;")?;
    source
        .select(item("reverse_dependencies").call("transitive_reverse_dependencies"))?
        .redirect("visible_reverse_dependencies")?;
    fs::write(search_rs, source.to_string())?;
    Ok(())
}

fn patch_symbol_index_source(symbol_index_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = Source::parse(&fs::read_to_string(symbol_index_rs)?, Edition::CURRENT)?;
    source
        .select(item("resolve_path_to_modules").parameter("db"))?
        .set_type("&RootDatabase")?;
    source
        .select(item("world_symbols").call("source_root_crates"))?
        .redirect("crate::visibility::source_root_crates")?;
    source
        .select(item("resolve_path_to_modules").call("Crate::all"))?
        .redirect("crate::visibility::all_hir_crates")?;
    source
        .select(item("resolve_path_to_modules").call("source_root_crates"))?
        .redirect("crate::visibility::source_root_crates")?;
    source
        .select(
            item("world_symbols")
                .call("search")
                .argument("cb")
                .closure(),
        )?
        .delegate("crate::visibility::visible_symbols", &["db"])?;
    fs::write(symbol_index_rs, source.to_string())?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
