use analyzed_bridge as build_support;
use r#override::{Source, item, root};

use std::{env, error::Error, path::PathBuf};

const PACKAGE: &str = "ra_ap_load-cargo";
const GENERATED_DIR: &str = "ra_ap_load_cargo_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut sources, _) =
        build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR, &[], &["ide-db"])?;
    sources.edit("src/lib.rs", patch_load_cargo_source)?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_load_cargo_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for path in [
        "ide_db::base_db::CrateBuilderId",
        "ide_db::base_db::ProcMacroPaths",
        "vfs::file_set::FileSet",
    ] {
        source.select(root())?.add_use(&format!("use {path};"))?;
    }
    source
        .select(root())?
        .mount_module("mod workspace_load", owned_source_path("workspace_load.rs"))?;
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
    source
        .select(item("load_crate_graph_into_db"))?
        .rename("_load_crate_graph_into_db")?;
    source
        .select(item("_load_crate_graph_into_db"))?
        .add_attribute("#[allow(dead_code)]")?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
