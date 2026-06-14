use std::{ops::Deref, panic::UnwindSafe};

use itertools::Itertools;
use lsp_types::DidSaveTextDocumentParams;
use vfs::{ChangeKind, VfsPath};

use crate::{
    flycheck::{InvocationStrategy, PackageSpecifier, Target},
    global_state::{FetchWorkspaceRequest, GlobalState},
    lsp::from_proto,
    main_loop::Task,
    reload,
    target_spec::TargetSpec,
    try_default,
};

pub(crate) fn handle_did_save_text_document(
    state: &mut GlobalState,
    params: DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    if let Ok(vfs_path) = from_proto::vfs_path(&params.text_document.uri) {
        let snap = state.snapshot();
        let file_id = try_default!(snap.vfs_path_to_file_id(&vfs_path)?);
        let sr = snap.analysis.source_root_id(file_id)?;

        if state.config.script_rebuild_on_save(Some(sr)) && state.build_deps_changed {
            state.build_deps_changed = false;
            state
                .fetch_build_data_queue
                .request_op("build_deps_changed - save notification".to_owned(), ());
        }

        // Re-fetch workspaces if a workspace related file has changed
        if let Some(path) = vfs_path.as_path() {
            let additional_files = &state
                .config
                .discover_workspace_config()
                .map(|cfg| cfg.files_to_watch.iter().map(String::as_str).collect::<Vec<&str>>())
                .unwrap_or_default();

            // FIXME: We should move this check into a QueuedTask and do semantic resolution of
            // the files. There is only so much we can tell syntactically from the path.
            if reload::should_refresh_for_change(path, ChangeKind::Modify, additional_files) {
                state.fetch_workspaces_queue.request_op(
                    format!("workspace vfs file change saved {path}"),
                    FetchWorkspaceRequest {
                        path: Some(path.to_owned()),
                        force_crate_graph_reload: false,
                    },
                );
            } else if state.detached_files.contains(path) {
                state.fetch_workspaces_queue.request_op(
                    format!("detached file saved {path}"),
                    FetchWorkspaceRequest {
                        path: Some(path.to_owned()),
                        force_crate_graph_reload: false,
                    },
                );
            }
        }

        if !state.config.check_on_save(Some(sr)) {
            return Ok(());
        }

        if run_flycheck(state, vfs_path) {
            state.diagnostics.clear_check_all();
            state.diagnostics.mark_changed(file_id);
            return Ok(());
        }
    } else if state.config.check_on_save(None) && state.config.flycheck_workspace(None) {
        // No specific flycheck was triggered, so let's trigger all of them.
        state.diagnostics.clear_check_all();
        for flycheck in state.flycheck.iter() {
            flycheck.restart_workspace(None);
        }
    }

    Ok(())
}

pub(crate) fn run_flycheck(state: &mut GlobalState, vfs_path: VfsPath) -> bool {
    let _p = tracing::info_span!("run_flycheck").entered();

    let base_file_id = state.analyzed_shared.base_vfs_path_to_file_id(&vfs_path);
    let file_id = state.analyzed_shared.vfs_path_to_file_id(&vfs_path);
    if let (Ok(Some(_)), Ok(Some(file_id))) = (base_file_id, file_id) {
        let analyzed_vfs_path = vfs_path.clone();
        let world = state.snapshot();
        let invocation_strategy = state.config.flycheck(None).invocation_strategy();
        let may_flycheck_workspace = state.config.flycheck_workspace(None);

        let task: Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe> =
            match invocation_strategy {
                InvocationStrategy::Once => Box::new(move || {
                    // FIXME: Because triomphe::Arc's auto UnwindSafe impl requires that the inner type
                    // be UnwindSafe, and FlycheckHandle is not UnwindSafe, `word.flycheck` cannot
                    // be captured directly. std::sync::Arc has an UnwindSafe impl that only requires
                    // that the inner type be RefUnwindSafe, so if we were using that one we wouldn't
                    // have this problem. Remove the line below when triomphe::Arc has an UnwindSafe impl
                    // like std::sync::Arc's.
                    let world = world;
                    stdx::always!(
                        world.flycheck.len() == 1,
                        "should have exactly one flycheck handle when invocation strategy is once"
                    );
                    let saved_file = vfs_path.as_path().map(ToOwned::to_owned);
                    world.flycheck[0].restart_workspace(saved_file);
                    Ok(())
                }),
                InvocationStrategy::PerWorkspace => Box::new(move || {
                    let saved_file = vfs_path.as_path().map(ToOwned::to_owned);
                    let target = TargetSpec::for_file(&world, file_id)?.map(|it| {
                        let tgt_kind = it.target_kind();
                        let (tgt_name, root, package) = match it {
                            TargetSpec::Cargo(c) => (
                                Some(c.target),
                                c.workspace_root,
                                PackageSpecifier::Cargo { package_id: c.package_id },
                            ),
                            TargetSpec::ProjectJson(p) => (
                                None,
                                p.project_root,
                                PackageSpecifier::BuildInfo { label: p.label.clone() },
                            ),
                        };

                        let tgt = tgt_name.and_then(|tgt_name| {
                            Some(match tgt_kind {
                                project_model::TargetKind::Bin => Target::Bin(tgt_name),
                                project_model::TargetKind::Example => Target::Example(tgt_name),
                                project_model::TargetKind::Test => Target::Test(tgt_name),
                                project_model::TargetKind::Bench => Target::Benchmark(tgt_name),
                                _ => return None,
                            })
                        });

                        (tgt, root, package)
                    });
                    tracing::debug!(?target, "flycheck target");
                    // we have a specific non-library target, attempt to only check that target, nothing
                    // else will be affected
                    let mut package_workspace_idx = None;
                    let mut package_check_triggered = false;
                    if let Some((target, root, package)) = target {
                        // trigger a package check if we have a non-library target as that can't affect
                        // anything else in the workspace OR if we're not allowed to check the workspace as
                        // the user opted into package checks then OR if this is not cargo.
                        let package_check_allowed = target.is_some()
                            || !may_flycheck_workspace
                            || matches!(package, PackageSpecifier::BuildInfo { .. });
                        if package_check_allowed {
                            package_workspace_idx =
                                world.workspaces.iter().position(|ws| match &ws.kind {
                                    project_model::ProjectWorkspaceKind::Cargo {
                                        cargo,
                                        ..
                                    }
                                    | project_model::ProjectWorkspaceKind::DetachedFile {
                                        cargo: Some((cargo, _, _)),
                                        ..
                                    } => *cargo.workspace_root() == root,
                                    project_model::ProjectWorkspaceKind::Json(p) => {
                                        *p.project_root() == root
                                    }
                                    project_model::ProjectWorkspaceKind::DetachedFile {
                                        cargo: None,
                                        ..
                                    } => false,
                                });
                            if let Some(idx) = package_workspace_idx {
                                // flycheck handles are indexed by their ID (which is the workspace index),
                                // but not all workspaces have flycheck enabled (e.g., JSON projects without
                                // a flycheck template). Find the flycheck handle by its ID.
                                if let Some(flycheck) =
                                    world.flycheck.iter().find(|fc| fc.id() == idx)
                                {
                                    let workspace_deps =
                                        world.all_workspace_dependencies_for_package(&package);
                                    flycheck.restart_for_package(
                                        package,
                                        target,
                                        workspace_deps,
                                        saved_file.clone(),
                                    );
                                    package_check_triggered = true;
                                }
                            }
                        }
                    }

                    if !may_flycheck_workspace {
                        return Ok(());
                    }

                    // Trigger flychecks for all workspaces that depend on the saved file
                    // Crates containing or depending on the saved file
                    let crate_ids: Vec<_> = world
                        .analysis
                        .crates_for(file_id)?
                        .into_iter()
                        .flat_map(|id| world.analysis.transitive_rev_deps(id))
                        .flatten()
                        .unique()
                        .collect();
                    tracing::debug!(?crate_ids, "flycheck crate ids");
                    let crate_root_paths: Vec<_> = crate_ids
                        .iter()
                        .filter_map(|&crate_id| {
                            world
                                .analysis
                                .crate_root(crate_id)
                                .map(|file_id| {
                                    world
                                        .file_id_to_file_path(file_id)
                                        .as_path()
                                        .map(ToOwned::to_owned)
                                })
                                .transpose()
                        })
                        .collect::<ide::Cancellable<_>>()?;
                    let crate_root_paths: Vec<_> =
                        crate_root_paths.iter().map(Deref::deref).collect();
                    tracing::debug!(?crate_root_paths, "flycheck crate roots");

                    // Find all workspaces that have at least one target containing the saved file
                    let workspace_ids = world.workspaces.iter().enumerate().filter(|&(idx, ws)| {
                        let ws_contains_file = match &ws.kind {
                            project_model::ProjectWorkspaceKind::Cargo {
                                cargo, ..
                            }
                            | project_model::ProjectWorkspaceKind::DetachedFile {
                                cargo: Some((cargo, _, _)),
                                ..
                            } => cargo.packages().any(|pkg| {
                                cargo[pkg]
                                    .targets
                                    .iter()
                                    .any(|&it| crate_root_paths.contains(&cargo[it].root.as_path()))
                            }),
                            project_model::ProjectWorkspaceKind::Json(project) => {
                                project.crates().any(|(_, krate)| {
                                    crate_root_paths.contains(&krate.root_module.as_path())
                                })
                            }
                            project_model::ProjectWorkspaceKind::DetachedFile {
                                ..
                            } => false,
                        };
                        let is_pkg_ws = package_check_triggered
                            && match package_workspace_idx {
                                Some(pkg_idx) => pkg_idx == idx,
                                None => false,
                            };
                        ws_contains_file && !is_pkg_ws
                    });

                    let mut workspace_check_triggered = false;
                    // Find and trigger corresponding flychecks
                    'flychecks: for flycheck in world.flycheck.iter() {
                        for (id, _) in workspace_ids.clone() {
                            if id == flycheck.id() {
                                workspace_check_triggered = true;
                                flycheck.restart_workspace(saved_file.clone());
                                continue 'flychecks;
                            }
                        }
                    }

                    // No specific flycheck was triggered, so let's trigger all of them.
                    if !workspace_check_triggered && !package_check_triggered {
                        for flycheck in world.flycheck.iter() {
                            flycheck.restart_workspace(saved_file.clone());
                        }
                    }
                    Ok(())
                }),
            };

        state
            .task_pool
            .handle
            .spawn_with_sender(stdx::thread::ThreadIntent::Worker, move |sender| {
                match std::panic::catch_unwind(task) {
                    Ok(Ok(())) => {}
                    Ok(Err(_cancelled)) => {
                        _ = sender.send(Task::AnalyzedRunFlycheck(analyzed_vfs_path));
                    }
                    Err(e) => tracing::error!("flycheck task panicked: {e:?}"),
                }
            });
        true
    } else {
        false
    }
}
