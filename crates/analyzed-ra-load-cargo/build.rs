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
use analyzed::analyzed_crate_graph_change;

// This variant of `load_workspace` allows deferring the loading of rust-analyzer
"#,
            analyzed.to_string_lossy().into_owned()
        ),
    )?;
    println!("cargo:rerun-if-changed={}", analyzed.display());

    replace_once(
        &mut source,
        r#"    let mut analysis_change = ChangeWithProcMacros::default();

    db.enable_proc_attr_macros();

    // wait until Vfs has loaded all roots
    for task in receiver {
        match task {
            vfs::loader::Message::Progress { n_done, .. } => {
                if n_done == LoadingProgress::Finished {
                    break;
                }
            }
            vfs::loader::Message::Loaded { files } | vfs::loader::Message::Changed { files } => {
                let _p =
                    tracing::info_span!("load_cargo::load_crate_craph/LoadedChanged").entered();
                for (path, contents) in files {
                    vfs.set_file_contents(path.into(), contents);
                }
            }
        }
    }
    let changes = vfs.take_changes();
    for (_, file) in changes {
        if let vfs::Change::Create(v, _) | vfs::Change::Modify(v, _) = file.change
            && let Ok(text) = String::from_utf8(v)
        {
            analysis_change.change_file(file.file_id, Some(text))
        }
    }
    let source_roots = source_root_config.partition(vfs);
    analysis_change.set_roots(source_roots);

    analysis_change.set_crate_graph(crate_graph);
    analysis_change.set_proc_macros(proc_macros);

    db.apply_change(analysis_change);
"#,
        r#"    let mut file_id_map = FxHashMap::default();
    let mut allocate_file_id = |file_id| file_id;
    let (analysis_change, _, _) = analyzed_crate_graph_change(
        crate_graph,
        proc_macros,
        source_root_config,
        vfs,
        receiver,
        &mut file_id_map,
        &mut allocate_file_id,
    );
    db.enable_proc_attr_macros();
    db.apply_change(analysis_change);
"#,
    )?;

    fs::write(lib_rs, source)?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
