use super::*;

pub type AnalyzedProcMacroLoad = (CrateBuilderId, ProcMacroLoadResult);

pub struct AnalyzedWorkspaceLoad {
    pub change: ChangeWithProcMacros,
    pub crate_graph: CrateGraphBuilder,
    pub proc_macros: Vec<AnalyzedProcMacroLoad>,
    pub source_roots: Vec<SourceRoot>,
    pub vfs: vfs::Vfs,
    pub file_id_map: FxHashMap<FileId, FileId>,
    pub file_texts: Vec<(FileId, String)>,
    pub source_root_parent_map: FxHashMap<SourceRootId, SourceRootId>,
    pub proc_macro_server: Option<ProcMacroClient>,
}

pub fn analyzed_load_workspace_change(
    ws: ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
    mut allocate_file_id: impl FnMut(FileId) -> FileId,
) -> anyhow::Result<AnalyzedWorkspaceLoad> {
    let (sender, receiver) = unbounded();
    let mut vfs = vfs::Vfs::default();
    let mut loader = {
        let loader = vfs_notify::NotifyHandle::spawn(sender);
        Box::new(loader)
    };
    let mut file_id_map = FxHashMap::default();

    tracing::debug!(?load_config, "LoadCargoConfig");
    let proc_macro_server = spawn_proc_macro_server(&ws, extra_env, load_config);
    log_proc_macro_server(&ws, &proc_macro_server);

    let (crate_graph, proc_macros) = ws.to_crate_graph(
        &mut |path: &AbsPath| {
            let contents = loader.load_sync(path);
            let path = vfs::VfsPath::from(path.to_path_buf());
            vfs.set_file_contents(path.clone(), contents);
            vfs.file_id(&path).and_then(|(file_id, excluded)| {
                (excluded == vfs::FileExcluded::No)
                    .then(|| analyzed_file_id(file_id, &mut file_id_map, &mut allocate_file_id))
            })
        },
        extra_env,
    );
    let proc_macros = collect_proc_macros(&proc_macro_server, proc_macros);

    let project_folders = ProjectFolders::new(std::slice::from_ref(&ws), &[], None);
    let source_root_parent_map = project_folders.source_root_config.source_root_parent_map();
    loader.set_config(vfs::loader::Config {
        load: project_folders.load,
        watch: vec![],
        version: 0,
    });

    let (change, file_texts, source_roots) = analyzed_crate_graph_change(
        crate_graph.clone(),
        proc_macros.iter().cloned().collect(),
        project_folders.source_root_config,
        &mut vfs,
        &receiver,
        &mut file_id_map,
        &mut allocate_file_id,
    );

    Ok(AnalyzedWorkspaceLoad {
        change,
        crate_graph,
        proc_macros,
        source_roots,
        vfs,
        file_id_map,
        file_texts,
        source_root_parent_map,
        proc_macro_server: proc_macro_server.and_then(Result::ok),
    })
}

pub(crate) fn analyzed_crate_graph_change(
    crate_graph: CrateGraphBuilder,
    proc_macros: ProcMacrosBuilder,
    source_root_config: SourceRootConfig,
    vfs: &mut vfs::Vfs,
    receiver: &Receiver<vfs::loader::Message>,
    file_id_map: &mut FxHashMap<FileId, FileId>,
    allocate_file_id: &mut impl FnMut(FileId) -> FileId,
) -> (ChangeWithProcMacros, Vec<(FileId, String)>, Vec<SourceRoot>) {
    let mut analysis_change = ChangeWithProcMacros::default();
    let mut file_texts = Vec::new();

    drain_loader(receiver, vfs);
    let changes = vfs.take_changes();
    for (_, file) in changes {
        if let vfs::Change::Create(v, _) | vfs::Change::Modify(v, _) = file.change
            && let Ok(text) = String::from_utf8(v)
        {
            let file_id = analyzed_file_id(file.file_id, file_id_map, allocate_file_id);
            analysis_change.change_file(file_id, Some(text.clone()));
            file_texts.push((file_id, text));
        }
    }
    let source_roots: Vec<SourceRoot> = source_root_config
        .partition(vfs)
        .into_iter()
        .map(|root| analyzed_source_root(root, file_id_map, allocate_file_id))
        .collect();
    analysis_change.set_roots(source_roots.clone());

    analysis_change.set_crate_graph(crate_graph);
    analysis_change.set_proc_macros(proc_macros);

    (analysis_change, file_texts, source_roots)
}

pub(crate) fn load_crate_graph_into_db(
    crate_graph: CrateGraphBuilder,
    proc_macros: ProcMacrosBuilder,
    source_root_config: SourceRootConfig,
    vfs: &mut vfs::Vfs,
    receiver: &Receiver<vfs::loader::Message>,
    db: &mut RootDatabase,
) {
    let mut file_id_map = FxHashMap::default();
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
}

fn spawn_proc_macro_server(
    workspace: &ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
) -> Option<Result<ProcMacroClient, ProcMacroLoadingError>> {
    match &load_config.with_proc_macro_server {
        ProcMacroServerChoice::Sysroot => workspace
            .find_sysroot_proc_macro_srv()
            .map(|result| {
                result
                    .and_then(|path| {
                        ProcMacroClient::spawn(
                            &path,
                            extra_env,
                            workspace.toolchain.as_ref(),
                            load_config.proc_macro_processes,
                        )
                        .map_err(Into::into)
                    })
                    .map_err(proc_macro_loading_error)
            }),
        ProcMacroServerChoice::Explicit(path) => Some(
            ProcMacroClient::spawn(
                path,
                extra_env,
                workspace.toolchain.as_ref(),
                load_config.proc_macro_processes,
            )
            .map_err(proc_macro_loading_error),
        ),
        ProcMacroServerChoice::None => Some(Err(ProcMacroLoadingError::Disabled)),
    }
}

fn proc_macro_loading_error(error: impl ToString) -> ProcMacroLoadingError {
    ProcMacroLoadingError::ProcMacroSrvError(error.to_string().into_boxed_str())
}

fn log_proc_macro_server(
    workspace: &ProjectWorkspace,
    proc_macro_server: &Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
) {
    let manifest = workspace.manifest_or_root();
    match proc_macro_server {
        Some(Ok(server)) => {
            tracing::info!(%manifest, path=%server.server_path(), "Proc-macro server started")
        }
        Some(Err(error)) => tracing::info!(%manifest, %error, "Failed to start proc-macro server"),
        None => tracing::info!(%manifest, "No proc-macro server started"),
    }
}

fn collect_proc_macros(
    proc_macro_server: &Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
    proc_macro_paths: ProcMacroPaths,
) -> Vec<AnalyzedProcMacroLoad> {
    let server = match proc_macro_server {
        Some(Ok(server)) => Ok(server),
        Some(Err(error)) => {
            Err(ProcMacroLoadingError::ProcMacroSrvError(error.to_string().into_boxed_str()))
        }
        None => Err(ProcMacroLoadingError::ProcMacroSrvError(
            "proc-macro-srv is not running, workspace is missing a sysroot".into(),
        )),
    };

    proc_macro_paths
        .into_iter()
        .map(|(crate_id, dylib)| {
            let proc_macros = dylib.map_or_else(Err, |(_, path)| {
                server
                    .clone()
                    .and_then(|server| load_proc_macro(server, &path, &[]))
            });
            (crate_id, proc_macros)
        })
        .collect()
}

fn drain_loader(receiver: &Receiver<vfs::loader::Message>, vfs: &mut vfs::Vfs) {
    while let Ok(task) = receiver.recv() {
        match task {
            vfs::loader::Message::Progress { n_done: LoadingProgress::Finished, .. } => break,
            vfs::loader::Message::Progress { .. } => (),
            vfs::loader::Message::Loaded { files } | vfs::loader::Message::Changed { files } => {
                let _p =
                    tracing::info_span!("load_cargo::load_crate_craph/LoadedChanged").entered();
                for (path, contents) in files {
                    vfs.set_file_contents(path.into(), contents);
                }
            }
        }
    }
}

fn analyzed_file_id(
    file_id: FileId,
    file_id_map: &mut FxHashMap<FileId, FileId>,
    allocate_file_id: &mut impl FnMut(FileId) -> FileId,
) -> FileId {
    *file_id_map
        .entry(file_id)
        .or_insert_with(|| allocate_file_id(file_id))
}

fn analyzed_source_root(
    root: SourceRoot,
    file_id_map: &mut FxHashMap<FileId, FileId>,
    allocate_file_id: &mut impl FnMut(FileId) -> FileId,
) -> SourceRoot {
    let mut file_set = FileSet::default();
    for file_id in root.iter() {
        let mapped_file_id = analyzed_file_id(file_id, file_id_map, allocate_file_id);
        let path = root
            .path_for_file(&file_id)
            .expect("source root file must have a path")
            .clone();
        file_set.insert(mapped_file_id, path);
    }

    if root.is_library {
        SourceRoot::new_library(file_set)
    } else {
        SourceRoot::new_local(file_set)
    }
}
