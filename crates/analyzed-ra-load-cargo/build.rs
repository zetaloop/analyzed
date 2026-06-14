use analyzed_bridge as build_support;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use analyzed_bridge::replace_once;

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

    replace_once(
        &mut source,
        "    base_db::{CrateGraphBuilder, Env, ProcMacroLoadingError, SourceRoot, SourceRootId},\n",
        "    base_db::{\n        CrateBuilderId, CrateGraphBuilder, Env, ProcMacroLoadingError, SourceRoot,\n        SourceRootId,\n    },\n",
    )?;
    replace_once(
        &mut source,
        "    file_set::FileSetConfig,\n",
        "    file_set::{FileSet, FileSetConfig},\n",
    )?;

    let analyzed = owned_source_path("analyzed.rs");
    replace_once(
        &mut source,
        "\n// This variant of `load_workspace` allows deferring the loading of rust-analyzer\n",
        &format!(
            r#"
#[path = {:?}]
mod analyzed;
pub use analyzed::{{
    AnalyzedProcMacroLoad, AnalyzedWorkspaceLoad, analyzed_load_workspace_change,
}};
use analyzed::load_crate_graph_into_db;

// This variant of `load_workspace` allows deferring the loading of rust-analyzer
"#,
            analyzed.to_string_lossy().into_owned()
        ),
    )?;
    println!("cargo:rerun-if-changed={}", analyzed.display());
    build_support::rename_with_prefix(
        &mut source,
        "fn load_crate_graph_into_db(",
        "load_crate_graph_into_db",
        "_",
    )?;
    build_support::allow_dead_code(&mut source, "fn _load_crate_graph_into_db(")?;

    fs::write(lib_rs, source)?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
