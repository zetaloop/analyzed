use analyzed_bridge as build_support;
use analyzed_bridge::ast;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const PACKAGE: &str = "ra_ap_load-cargo";
const GENERATED_DIR: &str = "ra_ap_load_cargo_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR, &[])?;
    patch_load_cargo_source(&generated.join("src/lib.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_load_cargo_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    build_support::add_use(&mut source, None, "ide_db::base_db::CrateBuilderId")?;
    build_support::add_use(&mut source, None, "ide_db::base_db::ProcMacroPaths")?;
    build_support::add_use(&mut source, None, "vfs::file_set::FileSet")?;

    let workspace_load = owned_source_path("workspace_load.rs");
    build_support::mount_module(&mut source, None, "workspace_load", &workspace_load)?;
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
        build_support::add_use(&mut source, Some("pub"), &format!("workspace_load::{name}"))?;
    }
    build_support::add_use(
        &mut source,
        None,
        "workspace_load::load_crate_graph_into_db",
    )?;
    println!("cargo:rerun-if-changed={}", workspace_load.display());

    build_support::rename::<ast::Fn>(
        &mut source,
        "load_crate_graph_into_db",
        "_load_crate_graph_into_db",
    )?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_load_crate_graph_into_db",
        "#[allow(dead_code)]",
    )?;

    fs::write(lib_rs, source)?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
