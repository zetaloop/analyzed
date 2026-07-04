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
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_analyzed_workspace_load_source(&generated.join("src/lib.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_analyzed_workspace_load_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    build_support::add_use(&mut source, None, "ide_db::base_db::CrateBuilderId")?;
    build_support::add_use(&mut source, None, "ide_db::base_db::ProcMacroPaths")?;
    build_support::add_use(&mut source, None, "vfs::file_set::FileSet")?;

    let analyzed = owned_source_path("analyzed.rs");
    source.insert_str(
        0,
        &format!(
            "#[path = {:?}]\nmod analyzed;\npub use analyzed::{{\n    ProcMacroLoad, WorkspaceLoad, load_workspace_change,\n}};\nuse analyzed::load_crate_graph_into_db;\n\n",
            analyzed.to_string_lossy().into_owned()
        ),
    );
    println!("cargo:rerun-if-changed={}", analyzed.display());

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
