use analyzed_bridge as build_support;
use r#override::{Edition, Source, item, root};

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const PACKAGE: &str = "ra_ap_load-cargo";
const GENERATED_DIR: &str = "ra_ap_load_cargo_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) =
        build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR, &[], &["ide-db"])?;
    patch_load_cargo_source(&generated.join("src/lib.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_load_cargo_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = Source::parse(&fs::read_to_string(lib_rs)?, Edition::CURRENT)?;
    for path in [
        "ide_db::base_db::CrateBuilderId",
        "ide_db::base_db::ProcMacroPaths",
        "vfs::file_set::FileSet",
    ] {
        source.select(root())?.add_use(&format!("use {path};"))?;
    }
    let workspace_load = owned_source_path("workspace_load.rs");
    source
        .select(root())?
        .mount_module("mod workspace_load", &workspace_load)?;
    for name in [
        "ProcMacroLoad",
        "ProcMacroLoadState",
        "WorkspaceLoad",
        "collect_proc_macros",
        "load_workspace_change",
        "source_root_for_path",
        "source_roots_for_files",
        "workspace_source_root_config",
    ] {
        source
            .select(root())?
            .add_use(&format!("pub use workspace_load::{name};"))?;
    }
    source
        .select(root())?
        .add_use("use workspace_load::load_crate_graph_into_db;")?;
    println!("cargo:rerun-if-changed={}", workspace_load.display());
    source
        .select(item("load_crate_graph_into_db"))?
        .rename("_load_crate_graph_into_db")?;
    source
        .select(item("_load_crate_graph_into_db"))?
        .add_attribute("#[allow(dead_code)]")?;
    fs::write(lib_rs, source.to_string())?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
