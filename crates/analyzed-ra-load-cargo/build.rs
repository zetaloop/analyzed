use analyzed_bridge as build_support;

use std::{error::Error, fs, path::Path};

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
        "\n// This variant of `load_workspace` allows deferring the loading of rust-analyzer\n",
        r#"
pub struct AnalyzedWorkspaceLoad {
    pub change: ChangeWithProcMacros,
    pub vfs: vfs::Vfs,
    pub file_texts: Vec<(FileId, String)>,
    pub proc_macro_server: Option<ProcMacroClient>,
}

pub fn analyzed_load_workspace_change(
    ws: ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
) -> anyhow::Result<AnalyzedWorkspaceLoad> {
    let (sender, receiver) = unbounded();
    let mut vfs = vfs::Vfs::default();
    let mut loader = {
        let loader = vfs_notify::NotifyHandle::spawn(sender);
        Box::new(loader)
    };

    tracing::debug!(?load_config, "LoadCargoConfig");
    let proc_macro_server = match &load_config.with_proc_macro_server {
        ProcMacroServerChoice::Sysroot => ws.find_sysroot_proc_macro_srv().map(|it| {
            it.and_then(|it| {
                ProcMacroClient::spawn(
                    &it,
                    extra_env,
                    ws.toolchain.as_ref(),
                    load_config.proc_macro_processes,
                )
                .map_err(Into::into)
            })
            .map_err(|e| ProcMacroLoadingError::ProcMacroSrvError(e.to_string().into_boxed_str()))
        }),
        ProcMacroServerChoice::Explicit(path) => Some(
            ProcMacroClient::spawn(
                path,
                extra_env,
                ws.toolchain.as_ref(),
                load_config.proc_macro_processes,
            )
            .map_err(|e| ProcMacroLoadingError::ProcMacroSrvError(e.to_string().into_boxed_str())),
        ),
        ProcMacroServerChoice::None => Some(Err(ProcMacroLoadingError::Disabled)),
    };
    match &proc_macro_server {
        Some(Ok(server)) => {
            tracing::info!(manifest=%ws.manifest_or_root(), path=%server.server_path(), "Proc-macro server started")
        }
        Some(Err(e)) => {
            tracing::info!(manifest=%ws.manifest_or_root(), %e, "Failed to start proc-macro server")
        }
        None => {
            tracing::info!(manifest=%ws.manifest_or_root(), "No proc-macro server started")
        }
    }

    let (crate_graph, proc_macros) = ws.to_crate_graph(
        &mut |path: &AbsPath| {
            let contents = loader.load_sync(path);
            let path = vfs::VfsPath::from(path.to_path_buf());
            vfs.set_file_contents(path.clone(), contents);
            vfs.file_id(&path).and_then(|(file_id, excluded)| {
                (excluded == vfs::FileExcluded::No).then_some(file_id)
            })
        },
        extra_env,
    );
    let proc_macros = {
        let proc_macro_server = match &proc_macro_server {
            Some(Ok(it)) => Ok(it),
            Some(Err(e)) => {
                Err(ProcMacroLoadingError::ProcMacroSrvError(e.to_string().into_boxed_str()))
            }
            None => Err(ProcMacroLoadingError::ProcMacroSrvError(
                "proc-macro-srv is not running, workspace is missing a sysroot".into(),
            )),
        };
        proc_macros
            .into_iter()
            .map(|(crate_id, path)| {
                (
                    crate_id,
                    path.map_or_else(Err, |(_, path)| {
                        proc_macro_server.as_ref().map_err(Clone::clone).and_then(
                            |proc_macro_server| load_proc_macro(proc_macro_server, &path, &[]),
                        )
                    }),
                )
            })
            .collect()
    };

    let project_folders = ProjectFolders::new(std::slice::from_ref(&ws), &[], None);
    loader.set_config(vfs::loader::Config {
        load: project_folders.load,
        watch: vec![],
        version: 0,
    });

    let (change, file_texts) = analyzed_crate_graph_change(
        crate_graph,
        proc_macros,
        project_folders.source_root_config,
        &mut vfs,
        &receiver,
    );

    Ok(AnalyzedWorkspaceLoad {
        change,
        vfs,
        file_texts,
        proc_macro_server: proc_macro_server.and_then(Result::ok),
    })
}

// This variant of `load_workspace` allows deferring the loading of rust-analyzer
"#,
    )?;

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
        r#"    let (analysis_change, _) =
        analyzed_crate_graph_change(crate_graph, proc_macros, source_root_config, vfs, receiver);
    db.enable_proc_attr_macros();
    db.apply_change(analysis_change);
"#,
    )?;

    replace_once(
        &mut source,
        "\nfn expander_to_proc_macro(\n",
        r#"
fn analyzed_crate_graph_change(
    crate_graph: CrateGraphBuilder,
    proc_macros: ProcMacrosBuilder,
    source_root_config: SourceRootConfig,
    vfs: &mut vfs::Vfs,
    receiver: &Receiver<vfs::loader::Message>,
) -> (ChangeWithProcMacros, Vec<(FileId, String)>) {
    let mut analysis_change = ChangeWithProcMacros::default();
    let mut file_texts = Vec::new();

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
            analysis_change.change_file(file.file_id, Some(text.clone()));
            file_texts.push((file.file_id, text));
        }
    }
    let source_roots = source_root_config.partition(vfs);
    analysis_change.set_roots(source_roots);

    analysis_change.set_crate_graph(crate_graph);
    analysis_change.set_proc_macros(proc_macros);

    (analysis_change, file_texts)
}

fn expander_to_proc_macro(
"#,
    )?;

    fs::write(lib_rs, source)?;
    Ok(())
}
