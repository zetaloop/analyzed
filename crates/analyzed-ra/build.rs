use std::{
    env,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use analyzed_bridge as build_support;
use r#override::{Source, Sources, TargetKind, arm, item, root};

const RA_PACKAGE: &str = "ra_ap_rust-analyzer";
const RA_REPOSITORY: &str = "rust-lang/rust-analyzer";

fn main() -> Result<(), Box<dyn Error>> {
    let (mut sources, revision) = build_support::prepare_bridge_package(
        RA_PACKAGE,
        "ra_ap_rust_analyzer_bridge",
        &[(TargetKind::Test, "slow-tests")],
        &["ide", "ide-completion", "ide-db", "ide-ssr", "load-cargo"],
    )?;
    build_support::restore_rust_analyzer_source(sources.directory())?;
    let revision = revision
        .as_deref()
        .ok_or("ra_ap_rust-analyzer does not contain .cargo_vcs_info.json")?;
    let pinned = pinned_upstream("release")?;
    let pinned_tag = pinned_upstream("tag")?;
    let release = if offline_build() {
        pinned
    } else {
        match rust_analyzer_release(revision, &pinned_tag) {
            Ok(release) => {
                if release != pinned {
                    return Err(format!(
                        "[package.metadata.upstream] release is {pinned}, but the rust-analyzer \
                         release for commit {revision} is {release}"
                    )
                    .into());
                }
                release
            }
            Err(error) if error.is::<GithubUnavailable>() => {
                println!(
                    "cargo:warning=could not verify the pinned upstream release {pinned}: {error}"
                );
                pinned
            }
            Err(error) => return Err(error),
        }
    };
    sources.edit("src/config.rs", patch_config_source)?;
    sources.edit("src/discover.rs", patch_discover_source)?;
    sources.edit("src/diagnostics.rs", patch_diagnostics_source)?;
    sources.edit("src/global_state.rs", patch_global_state_source)?;
    let pool = sources.read("src/task_pool.rs")?;
    sources.edit("src/main_loop.rs", |source| {
        patch_main_loop_source(source, &pool)
    })?;
    sources.edit("src/op_queue.rs", patch_op_queue_source)?;
    sources.edit("src/reload.rs", patch_reload_source)?;
    sources.edit("src/session.rs", patch_session_source)?;
    sources.edit("src/task_pool.rs", patch_task_pool_source)?;
    sources.edit(
        "src/diagnostics/flycheck_to_proto.rs",
        patch_flycheck_to_proto_source,
    )?;
    sources.edit("src/handlers/dispatch.rs", patch_dispatch_source)?;
    sources.edit("src/handlers/notification.rs", patch_notification_source)?;
    sources.edit("src/handlers/request.rs", patch_request_source)?;
    sources.edit("src/bin/main.rs", patch_driver_source)?;
    write_root_module(&sources.path("src/root.rs")?, &sources.path("src/lib.rs")?)?;
    patch_slow_tests(&mut sources)?;
    write_slow_tests_wrapper(&sources.path("tests/slow-tests")?)?;
    println!("cargo:rustc-env=ANALYZED_RA_RELEASE_VERSION={}", release);
    println!("cargo:rustc-env=ANALYZED_RA_COMMIT_HASH={revision}");
    println!("cargo:rerun-if-env-changed=GITHUB_TOKEN");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn pinned_upstream(key: &str) -> Result<String, Box<dyn Error>> {
    let manifest_path = Path::new(&env::var("CARGO_MANIFEST_DIR")?).join("Cargo.toml");
    let manifest: toml::Table = toml::from_str(&fs::read_to_string(manifest_path)?)?;
    manifest
        .get("package")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("upstream"))
        .and_then(|value| value.get(key))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("Cargo.toml lacks a [package.metadata.upstream] {key}").into())
}

fn offline_build() -> bool {
    env::var_os("DOCS_RS").is_some()
        || env::var("CARGO_NET_OFFLINE").is_ok_and(|value| value == "true")
}

#[derive(Debug)]
struct GithubUnavailable(String);

impl fmt::Display for GithubUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for GithubUnavailable {}

fn rust_analyzer_release(revision: &str, tag: &str) -> Result<String, Box<dyn Error>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();
    verify_rust_analyzer_release_tag(&agent, revision, tag)?;
    let runs = github_get(
        &agent,
        &format!(
            "/repos/{RA_REPOSITORY}/actions/workflows/release.yaml/runs\
             ?head_sha={revision}&branch=release&status=success"
        ),
    )?;
    let numbers = runs
        .get("workflow_runs")
        .and_then(serde_json::Value::as_array)
        .ok_or("GitHub workflow runs response has no workflow_runs array")?
        .iter()
        .filter_map(|run| run.get("run_number")?.as_u64())
        .collect::<Vec<_>>();
    match numbers.as_slice() {
        [number] => Ok(format!("v0.3.{number}")),
        [] => Err(GithubUnavailable(format!(
            "no release workflow run records commit {revision}; GitHub retains run history \
             for about 400 days"
        ))
        .into()),
        numbers => Err(format!(
            "multiple release workflow runs record commit {revision}: {}",
            numbers
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into()),
    }
}

fn verify_rust_analyzer_release_tag(
    agent: &ureq::Agent,
    revision: &str,
    tag: &str,
) -> Result<(), Box<dyn Error>> {
    let reference = github_get(agent, &format!("/repos/{RA_REPOSITORY}/git/ref/tags/{tag}"))?;
    let object = reference
        .get("object")
        .ok_or("GitHub tag response has no object")?;
    if object.get("type").and_then(serde_json::Value::as_str) != Some("commit") {
        return Err(format!("rust-analyzer release tag {tag} is not a commit ref").into());
    }
    let tag_revision = object
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .ok_or("GitHub tag response has no commit SHA")?;
    if tag_revision != revision {
        return Err(format!(
            "rust-analyzer release tag {tag} points to commit {tag_revision}, not {revision}"
        )
        .into());
    }
    Ok(())
}

fn github_get(agent: &ureq::Agent, path: &str) -> Result<serde_json::Value, Box<dyn Error>> {
    let url = format!("https://api.github.com{path}");
    let mut request = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header(
            "User-Agent",
            format!("analyzed/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(403 | 429)) if env::var_os("GITHUB_TOKEN").is_none() => {
            return Err(GithubUnavailable(format!(
                "GitHub API request to {path} was rate limited (60 requests/hour unauthenticated); \
                 set GITHUB_TOKEN to raise the limit"
            ))
            .into());
        }
        Err(error @ ureq::Error::StatusCode(_)) => return Err(error.into()),
        Err(error) => {
            return Err(
                GithubUnavailable(format!("GitHub API request to {path} failed: {error}")).into(),
            );
        }
    };
    Ok(serde_json::from_str(
        &response.body_mut().read_to_string()?,
    )?)
}

fn write_root_module(root_rs: &Path, lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let shared_analyzer = owned_source_path("shared_analyzer.rs");
    let shared_global_state = owned_source_path("global_state.rs");
    let shared_reload = owned_source_path("reload.rs");
    let shared_notification = owned_source_path("handlers/notification.rs");
    let upstream_root = fs::read_to_string(lib_rs)?;
    let source = format!(
        r#"
#[path = {:?}]
pub mod shared_analyzer;

#[path = {:?}]
pub(crate) mod shared_global_state;

#[path = {:?}]
pub(crate) mod shared_reload;

#[path = {:?}]
pub(crate) mod shared_notification;

{upstream_root}

#[path = {:?}]
pub mod driver;

pub use shared_analyzer::{{
    RUST_ANALYZER_VERSION,
    SharedAnalyzerBackendKey, SharedAnalyzerCargoConfigKey, SharedAnalyzerDatabaseConfigKey,
    SharedAnalyzerBackendSnapshot, SharedAnalyzerLoadKey,
    SharedAnalyzerProcMacroServerKey, SharedAnalyzerRegistry,
    SharedAnalyzerWorldKey, SharedAnalyzerViewKey, WorkspaceSummary,
    run_shared_rust_analyzer_lsp_session, run_shared_rust_analyzer_lsp_session_with_config,
    shared_analyzer_registry,
}};
"#,
        shared_analyzer.to_string_lossy().into_owned(),
        shared_global_state.to_string_lossy().into_owned(),
        shared_reload.to_string_lossy().into_owned(),
        shared_notification.to_string_lossy().into_owned(),
        lib_rs
            .with_file_name("bin/main.rs")
            .to_string_lossy()
            .into_owned()
    );
    fs::write(root_rs, source)?;
    println!("cargo:rerun-if-changed={}", shared_analyzer.display());
    println!("cargo:rerun-if-changed={}", shared_global_state.display());
    println!("cargo:rerun-if-changed={}", shared_reload.display());
    println!("cargo:rerun-if-changed={}", shared_notification.display());
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}

fn patch_config_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for function in [
        "generate_package_json_config",
        "generate_config_documentation",
    ] {
        source.select(item(function))?.add_attribute(
            "#[ignore = \"regenerates files from the rust-analyzer source tree\"]",
        )?;
    }
    Ok(())
}

fn patch_discover_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(item("DiscoverArgument").variant("Buildfile"))?
        .add_attribute("#[allow(dead_code)]")?;
    Ok(())
}

fn patch_diagnostics_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(item("fetch_native_diagnostics"))?
        .rename("_fetch_native_diagnostics")?;
    source
        .select(root())?
        .add_use("pub(crate) use crate::main_loop::session::fetch_native_diagnostics;")?;
    Ok(())
}

fn patch_global_state_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for field in [
        "pub(crate) shared: crate::shared_analyzer::SharedAnalyzerRuntime",
        "pub(crate) reload_id: Option<u64>",
        "pub(crate) adopted: bool",
    ] {
        source
            .select(item("FetchWorkspaceResponse"))?
            .add_field(field)?;
    }
    for field in [
        "pub(crate) rebuild_id: Option<u64>",
        "pub(crate) reload: bool",
    ] {
        source
            .select(item("FetchBuildDataResponse"))?
            .add_field(field)?;
    }
    source
        .select(item("FetchWorkspaceResponse"))?
        .add_attribute("#[derive(Debug)]")?;
    source
        .select(item("GlobalStateSnapshot"))?
        .add_field("pub(crate) shared: crate::shared_analyzer::SharedAnalyzerRuntime")?;
    for field in ["mem_docs", "vfs", "minicore"] {
        source
            .select(item("GlobalStateSnapshot").field(field))?
            .set_visibility("pub(crate)")?;
    }
    source
        .select(item("GlobalState").field("last_gc_revision"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(item("GlobalState::new"))?
        .rename("new_with_shared")?;
    source
        .select(item("GlobalState::new_with_shared"))?
        .add_parameter("shared: crate::shared_analyzer::SharedAnalyzerRuntime")?;
    source
        .select(item("GlobalState::new_with_shared"))?
        .add_parameter("workspaces: Vec<ProjectWorkspace>")?;
    for (declaration, initializer) in [
        (
            "shared: crate::shared_analyzer::SharedAnalyzerRuntime",
            "shared",
        ),
        ("reload_workspace: bool", "reload_workspace: false"),
        ("rebuild_proc_macros: bool", "rebuild_proc_macros: false"),
        ("rebuild_queued: bool", "rebuild_queued: false"),
        (
            "rebuilding_proc_macros: Option<u64>",
            "rebuilding_proc_macros: None",
        ),
        ("proc_macro_rebuild_id: u64", "proc_macro_rebuild_id: 0"),
        (
            "rebuild_response_current: Option<u64>",
            "rebuild_response_current: None",
        ),
        (
            "build_data_response_current: bool",
            "build_data_response_current: false",
        ),
        ("build_data_adoption: bool", "build_data_adoption: false"),
        (
            "build_data_rebuild_id: Option<u64>",
            "build_data_rebuild_id: None",
        ),
        ("build_data_reload: bool", "build_data_reload: false"),
        ("build_data_generation: u64", "build_data_generation: 0"),
        (
            "build_data_operation: Option<crate::shared_analyzer::SharedAnalyzerOperationToken>",
            "build_data_operation: None",
        ),
        (
            "proc_macro_operation: Option<crate::shared_analyzer::SharedAnalyzerOperationToken>",
            "proc_macro_operation: None",
        ),
        ("reload_pending: bool", "reload_pending: false"),
        (
            "proc_macro_clients_failed: bool",
            "proc_macro_clients_failed: false",
        ),
        ("workspace_reload_id: u64", "workspace_reload_id: 0"),
        (
            "handled_workspace_reload: Option<u64>",
            "handled_workspace_reload: None",
        ),
        (
            "workspace_adoption: Option<Arc<Vec<ProjectWorkspace>>>",
            "workspace_adoption: None",
        ),
    ] {
        source
            .select(item("GlobalState"))?
            .add_field(&format!("pub(crate) {declaration}"))?;
        source
            .select(item("GlobalState::new_with_shared").record("GlobalState"))?
            .add_field(initializer)?;
    }
    source
        .select(
            item("GlobalState::new_with_shared")
                .record("GlobalState")
                .field("workspaces"),
        )?
        .set_value("Arc::new(workspaces)")?;
    source
        .select(
            item("GlobalState::snapshot")
                .record("GlobalStateSnapshot")
                .field("analysis"),
        )?
        .set_value("self.shared.analysis()")?;
    source
        .select(item("GlobalState::snapshot").record("GlobalStateSnapshot"))?
        .add_field("shared: self.shared.clone()")?;
    source
        .select(item("GlobalStateSnapshot::target_spec_for_file"))?
        .rename("_target_spec_for_file")?;
    source
        .select(item("GlobalStateSnapshot::_target_spec_for_file"))?
        .add_attribute("#[allow(dead_code)]")?;
    source.select(item("GlobalStateSnapshot::_target_spec_for_file").region(root().for_loop().before(), root().end()))?
        .extract("pub(crate) fn target_spec_from_workspaces(&self, path: &paths::AbsPath, crate_id: Crate) -> Option<TargetSpec>", &["path", "crate_id"])?;
    source.select(item("GlobalState::compute_priming_scope").for_loop().has(arm("ProjectWorkspaceKind::Cargo")))?
        .extract("pub(crate) fn extend_priming_scope(&self, root_to_crate: &FxHashMap<AbsPathBuf, Vec<Crate>>, seed: &mut FxHashSet<Crate>)", &["&root_to_crate", "&mut seed"])?;
    for (owner, name) in [
        ("GlobalState", "compute_priming_scope"),
        ("GlobalState", "process_changes"),
        ("GlobalStateSnapshot", "url_to_file_id"),
        ("GlobalStateSnapshot", "file_id_to_url"),
        ("GlobalStateSnapshot", "vfs_path_to_file_id"),
        ("GlobalStateSnapshot", "file_line_index"),
        ("GlobalStateSnapshot", "file_version"),
        ("GlobalStateSnapshot", "anchored_path"),
        ("GlobalStateSnapshot", "file_id_to_file_path"),
        ("GlobalStateSnapshot", "file_exists"),
    ] {
        source
            .select(item(&format!("{owner}::{name}")))?
            .rename(&format!("_{name}"))?;
        source
            .select(item(&format!("{owner}::_{name}")))?
            .add_attribute("#[allow(dead_code)]")?;
    }
    source
        .select(item("enqueue_workspace_fetch"))?
        .set_visibility("pub(crate)")?;
    Ok(())
}

fn patch_main_loop_source(source: &mut Source, pool: &Source) -> Result<(), Box<dyn Error>> {
    let spawn = pool.declaration(root().implementation("TaskPool<T>").item("spawn"))?;
    let spawn_with_sender = pool.declaration(
        root()
            .implementation("TaskPool<T>")
            .item("spawn_with_sender"),
    )?;
    source.select(root())?.add_use("pub use crate::shared_analyzer::run_shared_rust_analyzer_lsp_session_with_config as main_loop;")?;
    source.select(item("main_loop"))?.rename("_main_loop")?;
    source
        .select(item("_main_loop"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(item("_main_loop"))?
        .set_visibility("pub(crate)")?;
    source.select(item("GlobalState::run"))?.extract("pub(crate) fn run_loop(&mut self, inbox: Receiver<lsp_server::Message>) -> anyhow::Result<()>", &["inbox"])?;
    source.select(item("GlobalState::run"))?.rename("_run")?;
    source
        .select(item("GlobalState::_run"))?
        .add_attribute("#[allow(dead_code)]")?;
    source.select(item("Event"))?.set_visibility("pub(crate)")?;
    for variant in [
        "FetchedWorkspace(FetchWorkspaceResponse)",
        "FetchedProcMacros(crate::shared_analyzer::SharedProcMacroProgress)",
        "SharedReloadReady(crate::shared_analyzer::SharedAnalyzerOperationToken)",
        "SharedRebuildReady(crate::shared_analyzer::SharedAnalyzerOperationToken)",
        "SharedBuildDataReady(String, crate::shared_analyzer::SharedAnalyzerOperationToken)",
        "SharedProcMacrosReady(String, crate::shared_analyzer::SharedAnalyzerOperationToken)",
        "WorkspaceUpdated(crate::shared_analyzer::SharedAnalyzerRuntime)",
        "RetryDeferred(DeferredTask)",
        "RetryDiscoverTests(Vec<FileId>)",
    ] {
        source.select(item("Task"))?.add_variant(variant)?;
    }
    source
        .select(item("DiscoverProjectParam").variant("Buildfile"))?
        .add_attribute("#[allow(dead_code)]")?;
    for name in [
        "handle_event",
        "update_diagnostics",
        "update_tests",
        "handle_task",
    ] {
        source.select(item(name))?.rename(&format!("_{name}"))?;
    }
    source.select(item("GlobalState::_update_diagnostics").body().region(root().child(root().binding("subscriptions")).after(), root().end()))?
        .extract("fn spawn_native_diagnostics(&mut self, generation: DiagnosticsGeneration, subscriptions: std::sync::Arc<[FileId]>)", &["generation", "subscriptions"])?;
    source
        .select(item("GlobalState::_update_diagnostics"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(item("GlobalState::_update_tests").body().region(
            root().child(root().binding("subscriptions")).after(),
            root().end(),
        ))?
        .extract(
            "fn spawn_discover_tests(&mut self, subscriptions: Vec<FileId>)",
            &["subscriptions"],
        )?;
    source
        .select(item("GlobalState::_update_tests"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(
            item("GlobalState::spawn_native_diagnostics")
                .for_loop()
                .call("snapshot"),
        )?
        .redirect("pending_snapshot")?;

    let task = item("GlobalState::spawn_discover_tests")
        .call("spawn")
        .declared_by(&spawn)
        .argument("task");
    source
        .select(task.clone().call("snapshot"))?
        .redirect("pending_snapshot")?;
    let callback = task.child(root().closure());
    source
        .select(callback.clone())?
        .add_parameter("snapshot: _")?;
    source.select(callback)?.delegate(
        "crate::main_loop::session::discover_tests",
        &["snapshot", "subscriptions.clone()"],
    )?;

    let task = item("GlobalState::prime_caches")
        .call("spawn_with_sender")
        .declared_by(&spawn_with_sender)
        .argument("task");
    source
        .select(task.clone().call("snapshot"))?
        .redirect("pending_snapshot")?;
    let callback = task.child(root().closure());
    source
        .select(callback.clone())?
        .at(root().parameter("sender").before())
        .add_parameter("analysis: _")?;
    source
        .select(callback)?
        .delegate("crate::main_loop::session::prime_caches", &["analysis"])?;

    for (variant, helper, context, parameter) in [
        (
            "DeferredTask::CheckIfIndexed",
            "crate::main_loop::session::check_if_indexed",
            &["snap", "uri.clone()"][..],
            "snap: _",
        ),
        (
            "DeferredTask::CheckProcMacroSources",
            "crate::main_loop::session::check_proc_macro_sources",
            &["analysis", "modified_rust_files.clone()"][..],
            "analysis: _",
        ),
    ] {
        let scope = item("GlobalState::handle_deferred_task").arm(variant);
        source
            .select(scope.clone().call("snapshot"))?
            .redirect("pending_snapshot")?;
        let callback = scope
            .call("spawn_with_sender")
            .declared_by(&spawn_with_sender)
            .argument("task")
            .closure()
            .has(root().parameter("sender"));
        source
            .select(callback.clone())?
            .at(root().parameter("sender").before())
            .add_parameter(parameter)?;
        source.select(callback)?.delegate(helper, context)?;
    }

    source
        .select(
            item("GlobalState::_handle_event")
                .arm("PrimeCachesProgress::End")
                .call("trigger_garbage_collection"),
        )?
        .extract("fn mark_prime_caches_gc(&mut self)", &[])?;
    source
        .select(item("mark_prime_caches_gc"))?
        .rename("_mark_prime_caches_gc")?;
    source
        .select(item("_mark_prime_caches_gc"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(
            item("GlobalState::_handle_event")
                .if_expr()
                .has(root().condition().references("self.last_gc_revision"))
                .call("trigger_garbage_collection"),
        )?
        .extract("fn mark_idle_gc(&mut self)", &[])?;
    source
        .select(item("mark_idle_gc"))?
        .rename("_mark_idle_gc")?;
    source
        .select(item("_mark_idle_gc"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(
            item("GlobalState::_handle_event")
                .if_expr()
                .has(root().condition().call("take_changes"))
                .for_loop()
                .body(),
        )?
        .extract(
            "fn publish_changed_diagnostics(&mut self, file_id: FileId)",
            &["file_id"],
        )?;
    source
        .select(item("publish_changed_diagnostics"))?
        .rename("_publish_changed_diagnostics")?;
    source
        .select(item("_publish_changed_diagnostics"))?
        .add_attribute("#[allow(dead_code)]")?;
    source.select(item("GlobalState::handle_flycheck_msg").arm("FlycheckMessage::AddDiagnostic").for_loop().body())?
        .extract("fn record_flycheck_diagnostic(&mut self, id: usize, generation: DiagnosticsGeneration, package_id: Option<crate::flycheck::PackageSpecifier>, diag: crate::diagnostics::flycheck_to_proto::MappedRustDiagnostic)", &["id", "generation", "package_id.clone()", "diag"])?;
    source
        .select(item("record_flycheck_diagnostic"))?
        .rename("_record_flycheck_diagnostic")?;
    source
        .select(item("_record_flycheck_diagnostic"))?
        .add_attribute("#[allow(dead_code)]")?;
    for field in [
        "shared: self.shared.clone()",
        "reload_id: None",
        "adopted: false",
    ] {
        source
            .select(item("GlobalState::_handle_task").record("FetchWorkspaceResponse"))?
            .add_field(field)?;
    }
    for field in [
        "rebuild_id: self.build_data_rebuild_id",
        "reload: self.build_data_reload",
    ] {
        source
            .select(item("GlobalState::_handle_task").record("FetchBuildDataResponse"))?
            .add_field(field)?;
    }
    source
        .select(item("GlobalState::_handle_task").symbol("Task"))?
        .redirect("self::session::UpstreamTask")?;
    let session = owned_source_path("session.rs");
    source
        .select(root())?
        .mount_module("pub(crate) mod session", &session)?;
    Ok(())
}

fn patch_session_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source.select(item("IoThreads"))?.add_variant("External")?;
    source
        .select(
            item("IoThreads::join")
                .match_expr()
                .has(arm("IoThreads::Stdio")),
        )?
        .add_arm("IoThreads::External => Ok(())")?;
    Ok(())
}

fn patch_op_queue_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(item("OpQueue").field("last_op_result"))?
        .set_visibility("pub(crate)")?;
    Ok(())
}

fn patch_reload_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for name in [
        "update_configuration",
        "fetch_workspaces",
        "fetch_build_data",
        "fetch_proc_macros",
        "recreate_crate_graph",
    ] {
        source.select(item(name))?.rename(&format!("_{name}"))?;
    }
    for name in [
        "_fetch_workspaces",
        "_fetch_proc_macros",
        "_recreate_crate_graph",
    ] {
        source
            .select(item(name))?
            .add_attribute("#[allow(dead_code)]")?;
    }
    source
        .select(item("reload_flycheck"))?
        .set_visibility("pub(crate)")?;
    for response in ["FetchWorkspaceResponse", "FetchBuildDataResponse"] {
        source
            .select(item("GlobalState::switch_workspaces").pattern(response))?
            .add_rest()?;
    }
    source
        .select(
            item("GlobalState::switch_workspaces")
                .body()
                .child(root().call("recreate_crate_graph")),
        )?
        .redirect("recreate_crate_graph_from_shared")?;
    source
        .select(item("GlobalState::switch_workspaces"))?
        .rename("_switch_workspaces")?;
    source
        .select(
            item("GlobalState::_switch_workspaces")
                .if_expr()
                .has(root().condition().call("expand_proc_macros")),
        )?
        .extract(
            "fn set_proc_macro_clients(&mut self, same_workspaces: bool)",
            &["same_workspaces"],
        )?;
    source
        .select(item("set_proc_macro_clients"))?
        .rename("_set_proc_macro_clients")?;
    source
        .select(item("_set_proc_macro_clients"))?
        .add_attribute("#[allow(dead_code)]")?;
    Ok(())
}

fn patch_dispatch_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    let shared_dispatch = owned_source_path("shared_dispatch.rs");
    source
        .select(root())?
        .mount_module("mod shared_dispatch", &shared_dispatch)?;
    println!("cargo:rerun-if-changed={}", shared_dispatch.display());
    source
        .select(item("on_with_thread_intent").call("snapshot"))?
        .redirect("pending_snapshot")?;
    let callback = item("on_with_thread_intent")
        .call("spawn")
        .child(root().closure());
    source.select(callback.clone())?.add_parameter("world: _")?;
    source.select(callback)?.delegate(
        "crate::handlers::dispatch::shared_dispatch::on_with_thread_intent",
        &["world", "req.clone()"],
    )?;
    Ok(())
}

fn patch_request_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for name in ["handle_workspace_reload", "handle_proc_macros_rebuild"] {
        let replacement = format!("_{name}");
        source.select(item(name))?.rename(&replacement)?;
        source
            .select(item(&replacement))?
            .add_attribute("#[allow(dead_code)]")?;
        source
            .select(root())?
            .add_use(&format!("pub(crate) use crate::shared_reload::{name};"))?;
    }
    Ok(())
}

fn patch_flycheck_to_proto_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source.select(item("location"))?.rename("_location")?;
    source
        .select(root())?
        .add_use("use self::flycheck_location::location;")?;
    let flycheck_location = owned_source_path("diagnostics/flycheck_location.rs");
    source
        .select(root())?
        .mount_module("mod flycheck_location", &flycheck_location)?;
    println!("cargo:rerun-if-changed={}", flycheck_location.display());
    Ok(())
}

fn patch_notification_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(
            item("handle_did_close_text_document")
                .if_expr()
                .has(root().condition().references("state.vfs")),
        )?
        .extract(
            "fn clear_native_diagnostics_for_closed_file(state: &mut GlobalState, path: VfsPath)",
            &["state", "path.clone()"],
        )?;
    source
        .select(item("clear_native_diagnostics_for_closed_file"))?
        .rename("_clear_native_diagnostics_for_closed_file")?;
    source
        .select(item("_clear_native_diagnostics_for_closed_file"))?
        .add_attribute("#[allow(dead_code)]")?;
    source.select(root())?.add_use(
        "pub(crate) use crate::shared_notification::clear_native_diagnostics_for_closed_file;",
    )?;
    source
        .select(
            item("run_flycheck")
                .if_expr()
                .has(root().condition().pattern("FileExcluded::No"))
                .call("snapshot"),
        )?
        .redirect("pending_snapshot")?;
    source.select(item("run_flycheck").arm("InvocationStrategy::Once"))?
        .extract("pub(crate) fn run_flycheck_once(world: crate::shared_global_state::PendingGlobalStateSnapshot, vfs_path: vfs::VfsPath) -> Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe>", &["world", "vfs_path.clone()"])?;
    let callback = item("run_flycheck_once")
        .call("Box::new")
        .child(root().closure());
    source.select(callback.clone())?.add_parameter("world: _")?;
    source
        .select(callback)?
        .delegate("crate::shared_notification::activate_flycheck", &["world"])?;
    source.select(item("run_flycheck").arm("InvocationStrategy::PerWorkspace"))?
        .extract("pub(crate) fn run_flycheck_per_workspace(world: crate::shared_global_state::PendingGlobalStateSnapshot, file_id: ide::FileId, vfs_path: vfs::VfsPath, may_flycheck_workspace: bool) -> Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe>", &["world", "file_id", "vfs_path.clone()", "may_flycheck_workspace"])?;
    let callback = item("run_flycheck_per_workspace")
        .call("Box::new")
        .child(root().closure());
    source.select(callback.clone())?.add_parameter("world: _")?;
    source.select(callback)?.delegate(
        "crate::shared_notification::select_flycheck_per_workspace",
        &["world"],
    )?;
    source
        .select(item("run_flycheck"))?
        .set_visibility("pub(crate)")?;
    source
        .select(item("run_flycheck"))?
        .rename("_run_flycheck")?;
    source
        .select(item("_run_flycheck"))?
        .add_attribute("#[allow(dead_code)]")?;
    source
        .select(root())?
        .add_use("pub(crate) use crate::shared_notification::run_flycheck;")?;
    source
        .select(item("handle_did_save_text_document"))?
        .rename("_handle_did_save_text_document")?;
    source
        .select(root())?
        .add_use("pub(crate) use crate::shared_notification::handle_did_save_text_document;")?;
    Ok(())
}

fn patch_task_pool_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(item("TaskPool").field("sender"))?
        .set_visibility("pub(crate)")?;
    Ok(())
}

fn patch_driver_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for name in ["main", "setup_logging", "wait_for_debugger"] {
        source.select(item(name))?.set_visibility("pub")?;
    }
    source
        .select(root())?
        .add_use("use crate as rust_analyzer;")?;
    Ok(())
}

fn patch_slow_tests(sources: &mut Sources) -> Result<(), Box<dyn Error>> {
    let slow_tests = Path::new("tests/slow-tests");
    for name in ["main.rs", "ratoml.rs", "cli.rs", "flycheck.rs"] {
        sources.edit(slow_tests.join(name), |source| {
            source
                .select(root().import("skip_slow_tests"))?
                .redirect("crate::test_support::skip_slow_tests")
        })?;
    }
    sources.edit(slow_tests.join("support.rs"), |source| {
        source
            .select(item("lines_match"))?
            .rename("_original_lines_match")?;
        source
            .select(item("_original_lines_match"))?
            .set_visibility("pub(crate)")?;
        source
            .select(root())?
            .add_use("use crate::test_support::lines_match;")
    })?;
    sources.edit(slow_tests.join("ratoml.rs"), |source| {
        source
            .select(item("fixture_path"))?
            .rename("_original_fixture_path")?;
        source
            .select(root())?
            .mount_module("mod fixture_uri", owned_source_path("slow_tests_uri.rs"))?;
        source
            .select(root())?
            .add_use("use self::fixture_uri::FixturePath;")
    })
}

fn write_slow_tests_wrapper(slow_tests: &Path) -> Result<(), Box<dyn Error>> {
    let test_support = owned_source_path("slow_tests.rs");
    let main_rs = slow_tests.join("main.rs");
    let wrapper_rs = slow_tests.join("test-support.rs");
    fs::write(
        &wrapper_rs,
        format!(
            "extern crate ra_ap_rust_analyzer as rust_analyzer;\n#[path = {:?}]\nmod test_support;\ninclude!({:?});\n",
            test_support.to_string_lossy().into_owned(),
            main_rs.to_string_lossy().into_owned(),
        ),
    )?;
    println!("cargo:rerun-if-changed={}", test_support.display());
    Ok(())
}
