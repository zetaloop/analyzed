use super::*;

pub type ProcMacroLoad = (CrateBuilderId, ProcMacroLoadResult);

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProcMacroLoadState {
    NotYetBuilt,
    Ready,
    Disabled,
}

pub struct WorkspaceLoad {
    pub crate_graph: CrateGraphBuilder,
    pub proc_macro_paths: ProcMacroPaths,
    pub proc_macros: Vec<ProcMacroLoad>,
    pub source_roots: Vec<SourceRoot>,
    pub source_root_config: SourceRootConfig,
    pub vfs: vfs::Vfs,
    pub file_id_map: FxHashMap<FileId, FileId>,
    pub file_texts: Vec<(FileId, String)>,
    pub source_root_parent_map: FxHashMap<SourceRootId, SourceRootId>,
    pub proc_macro_server: Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
}

pub fn load_workspace_change(
    ws: ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
    ignored_proc_macros: &[(Box<str>, Vec<Box<str>>) ],
    proc_macro_server: Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
    proc_macro_state: ProcMacroLoadState,
    mut allocate_file_id: impl FnMut(FileId, &VfsPath) -> FileId,
) -> anyhow::Result<WorkspaceLoad> {
    let (sender, receiver) = unbounded();
    let mut vfs = vfs::Vfs::default();
    let mut loader = {
        let loader = vfs_notify::NotifyHandle::spawn(sender);
        Box::new(loader)
    };
    let mut file_id_map = FxHashMap::default();

    tracing::debug!(?load_config, "LoadCargoConfig");
    log_proc_macro_server(&ws, &proc_macro_server);

    let (crate_graph, proc_macro_paths) = ws.to_crate_graph(
        &mut |path: &AbsPath| {
            let contents = loader.load_sync(path);
            let path = vfs::VfsPath::from(path.to_path_buf());
            vfs.set_file_contents(path.clone(), contents);
            vfs.file_id(&path).and_then(|(file_id, excluded)| {
                (excluded == vfs::FileExcluded::No)
                    .then(|| {
                        analyzed_file_id(
                            file_id,
                            &path,
                            &mut file_id_map,
                            &mut allocate_file_id,
                        )
                    })
            })
        },
        extra_env,
    );
    let proc_macros = collect_proc_macros(
        &proc_macro_server,
        proc_macro_paths.clone(),
        ignored_proc_macros,
        proc_macro_state,
        &|_| {},
    );
    let project_folders = ProjectFolders::new(std::slice::from_ref(&ws), &[], None);
    let source_root_config = project_folders.source_root_config;
    let source_root_parent_map = source_root_config.source_root_parent_map();
    loader.set_config(vfs::loader::Config {
        load: project_folders.load,
        watch: vec![],
        version: 0,
    });

    let (_, file_texts, source_roots) = crate_graph_change(
        crate_graph.clone(),
        proc_macros.iter().cloned().collect(),
        &source_root_config,
        &mut vfs,
        &receiver,
        &mut file_id_map,
        &mut allocate_file_id,
    );

    Ok(WorkspaceLoad {
        crate_graph,
        proc_macro_paths,
        proc_macros,
        source_roots,
        source_root_config,
        vfs,
        file_id_map,
        file_texts,
        source_root_parent_map,
        proc_macro_server,
    })
}

pub(crate) fn crate_graph_change(
    crate_graph: CrateGraphBuilder,
    proc_macros: ProcMacrosBuilder,
    source_root_config: &SourceRootConfig,
    vfs: &mut vfs::Vfs,
    receiver: &Receiver<vfs::loader::Message>,
    file_id_map: &mut FxHashMap<FileId, FileId>,
    allocate_file_id: &mut impl FnMut(FileId, &VfsPath) -> FileId,
) -> (ChangeWithProcMacros, Vec<(FileId, String)>, Vec<SourceRoot>) {
    let mut analysis_change = ChangeWithProcMacros::default();
    let mut file_texts = Vec::new();

    drain_loader(receiver, vfs);
    let changes = vfs.take_changes();
    for (_, file) in changes {
        if let vfs::Change::Create(v, _) | vfs::Change::Modify(v, _) = file.change
            && let Ok(text) = String::from_utf8(v)
        {
            let file_id = analyzed_file_id(
                file.file_id,
                vfs.file_path(file.file_id),
                file_id_map,
                allocate_file_id,
            );
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

pub fn workspace_source_root_config(workspace: &ProjectWorkspace) -> SourceRootConfig {
    ProjectFolders::new(std::slice::from_ref(workspace), &[], None).source_root_config
}

pub fn source_root_for_path(config: &SourceRootConfig, path: &VfsPath) -> Option<bool> {
    config
        .fsc
        .classify_path(path)
        .map(|index| !config.local_filesets.contains(&(index as u64)))
}

pub fn source_roots_for_files(
    config: &SourceRootConfig,
    files: impl IntoIterator<Item = (FileId, VfsPath)>,
) -> Vec<SourceRoot> {
    let mut vfs = vfs::Vfs::default();
    let mut file_id_map = FxHashMap::default();
    for (file_id, path) in files {
        vfs.set_file_contents(path.clone(), Some(Vec::new()));
        let (vfs_file_id, _) = vfs.file_id(&path).expect("source-root file must exist");
        file_id_map.insert(vfs_file_id, file_id);
    }

    config
        .partition(&vfs)
        .into_iter()
        .map(|root| analyzed_source_root(root, &mut file_id_map, &mut |file_id, _| file_id))
        .collect()
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
    let mut allocate_file_id = |file_id, _: &VfsPath| file_id;
    let (analysis_change, _, _) = crate_graph_change(
        crate_graph,
        proc_macros,
        &source_root_config,
        vfs,
        receiver,
        &mut file_id_map,
        &mut allocate_file_id,
    );
    db.enable_proc_attr_macros();
    db.apply_change(analysis_change);
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

pub fn collect_proc_macros(
    proc_macro_server: &Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
    proc_macro_paths: ProcMacroPaths,
    ignored_proc_macros: &[(Box<str>, Vec<Box<str>>) ],
    state: ProcMacroLoadState,
    progress: &dyn Fn(&AbsPath),
) -> Vec<ProcMacroLoad> {
    let server = match state {
        ProcMacroLoadState::NotYetBuilt => Err(ProcMacroLoadingError::NotYetBuilt),
        ProcMacroLoadState::Disabled => Err(ProcMacroLoadingError::Disabled),
        ProcMacroLoadState::Ready => match proc_macro_server {
            Some(Ok(server)) => Ok(server),
            Some(Err(error)) => {
                Err(ProcMacroLoadingError::ProcMacroSrvError(error.to_string().into_boxed_str()))
            }
            None => Err(ProcMacroLoadingError::ProcMacroSrvError(
                "proc-macro-srv is not running, workspace is missing a sysroot".into(),
            )),
        },
    };

    proc_macro_paths
        .into_iter()
        .map(|(crate_id, dylib)| {
            let proc_macros = dylib.map_or_else(Err, |(crate_name, path)| {
                progress(&path);
                let ignored = ignored_proc_macros
                    .iter()
                    .find_map(|(name, macros)| eq_ignore_underscore(name, &crate_name).then_some(macros))
                    .map_or(&[][..], Vec::as_slice);
                server
                    .clone()
                    .and_then(|server| load_proc_macro(server, &path, ignored))
            });
            (crate_id, proc_macros)
        })
        .collect()
}

fn eq_ignore_underscore(s1: &str, s2: &str) -> bool {
    s1.len() == s2.len()
        && s1.as_bytes().iter().zip(s2.as_bytes()).all(|(c1, c2)| {
            let c1_underscore = c1 == &b'_' || c1 == &b'-';
            let c2_underscore = c2 == &b'_' || c2 == &b'-';
            c1 == c2 || (c1_underscore && c2_underscore)
        })
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
    path: &VfsPath,
    file_id_map: &mut FxHashMap<FileId, FileId>,
    allocate_file_id: &mut impl FnMut(FileId, &VfsPath) -> FileId,
) -> FileId {
    *file_id_map
        .entry(file_id)
        .or_insert_with(|| allocate_file_id(file_id, path))
}

fn analyzed_source_root(
    root: SourceRoot,
    file_id_map: &mut FxHashMap<FileId, FileId>,
    allocate_file_id: &mut impl FnMut(FileId, &VfsPath) -> FileId,
) -> SourceRoot {
    let mut file_set = FileSet::default();
    for file_id in root.iter() {
        let path = root
            .path_for_file(&file_id)
            .expect("source root file must have a path")
            .clone();
        let mapped_file_id =
            analyzed_file_id(file_id, &path, file_id_map, allocate_file_id);
        file_set.insert(mapped_file_id, path);
    }

    if root.is_library {
        SourceRoot::new_library(file_set)
    } else {
        SourceRoot::new_local(file_set)
    }
}
