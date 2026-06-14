use std::{
    env,
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use toml::{Table, Value, map::Map};

use analyzed_bridge as build_support;

const RA_PACKAGE: &str = "ra_ap_rust-analyzer";

fn main() -> Result<(), Box<dyn Error>> {
    let (generated, package) =
        build_support::prepare_bridge_package(RA_PACKAGE, "ra_ap_rust_analyzer_bridge")?;
    verify_manifest_matches_bridge(&generated.join("Cargo.toml"))?;
    let generated_src = generated.join("src");
    patch_global_state_source(&generated_src.join("global_state.rs"))?;
    patch_config_source(&generated_src.join("config.rs"))?;
    patch_discover_source(&generated_src.join("discover.rs"))?;
    patch_flycheck_to_proto_source(&generated_src.join("diagnostics/flycheck_to_proto.rs"))?;
    patch_notification_source(&generated_src.join("handlers/notification.rs"))?;
    patch_dispatch_source(&generated_src.join("handlers/dispatch.rs"))?;
    patch_main_loop_source(&generated_src.join("main_loop.rs"))?;
    patch_reload_source(&generated_src.join("reload.rs"))?;
    patch_test_tool_attributes(&generated_src)?;
    append_main_loop_session_module(&generated_src.join("main_loop.rs"))?;
    write_analyzed_root_module(
        &generated_src.join("analyzed_root.rs"),
        &generated_src.join("lib.rs"),
    )?;
    let slow_tests = generated.join("tests/slow-tests");
    patch_slow_tests(&slow_tests)?;
    let slow_tests_wrapper = write_slow_tests_wrapper(&slow_tests)?;
    println!("cargo:rustc-env=ANALYZED_RA_VERSION={}", package.version);
    println!(
        "cargo:rustc-env=ANALYZED_RA_SLOW_TESTS={}",
        slow_tests_wrapper.display()
    );
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn verify_manifest_matches_bridge(ra_manifest_path: &Path) -> Result<(), Box<dyn Error>> {
    let bridge_manifest_path = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    )
    .join("Cargo.toml");
    let ra_manifest = read_manifest(ra_manifest_path)?;
    let bridge_manifest = read_manifest(&bridge_manifest_path)?;
    let mut mismatches = Vec::new();

    compare_manifest_section(
        "dependencies",
        normalized_dependencies(&ra_manifest),
        normalized_dependencies(&bridge_manifest),
        &mut mismatches,
    );
    compare_manifest_section(
        "target",
        normalized_target_dependencies(&ra_manifest),
        normalized_target_dependencies(&bridge_manifest),
        &mut mismatches,
    );
    compare_manifest_section(
        "features",
        manifest_section(&ra_manifest, &["features"]),
        manifest_section(&bridge_manifest, &["features"]),
        &mut mismatches,
    );
    compare_manifest_section(
        "lints",
        manifest_section(&ra_manifest, &["lints"]),
        manifest_section(&bridge_manifest, &["lints"]),
        &mut mismatches,
    );

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "ra_ap_rust-analyzer bridge Cargo.toml is out of sync with {RA_PACKAGE}:\n{}",
            mismatches.join("\n")
        )
        .into())
    }
}

fn read_manifest(path: &Path) -> Result<Value, Box<dyn Error>> {
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

fn manifest_table<'a>(manifest: &'a Value, path: &[&str]) -> Option<&'a Table> {
    let mut value = manifest;
    for key in path {
        value = value.get(*key)?;
    }
    value.as_table()
}

fn manifest_section(manifest: &Value, path: &[&str]) -> Option<Value> {
    let mut value = manifest;
    for key in path {
        value = value.get(*key)?;
    }
    Some(value.clone())
}

fn compare_manifest_section(
    label: &str,
    expected: Option<Value>,
    actual: Option<Value>,
    mismatches: &mut Vec<String>,
) {
    match (expected, actual) {
        (Some(expected), Some(actual)) if expected == actual => {}
        (Some(Value::Table(expected)), Some(Value::Table(actual))) => {
            compare_manifest_tables(label, &expected, &actual, mismatches);
        }
        (Some(_), Some(_)) => mismatches.push(format!("  {label}: different value")),
        (Some(_), None) => {
            mismatches.push(format!("  {label}: missing section in bridge manifest"))
        }
        (None, Some(_)) => mismatches.push(format!("  {label}: extra section in bridge manifest")),
        (None, None) => {}
    }
}

fn compare_manifest_tables(
    label: &str,
    expected: &Table,
    actual: &Table,
    mismatches: &mut Vec<String>,
) {
    let expected_only = table_keys(expected)
        .into_iter()
        .filter(|key| !actual.contains_key(key))
        .collect::<Vec<_>>();
    let actual_only = table_keys(actual)
        .into_iter()
        .filter(|key| !expected.contains_key(key))
        .collect::<Vec<_>>();
    let changed = table_keys(expected)
        .into_iter()
        .filter(|key| actual.get(key) != expected.get(key))
        .collect::<Vec<_>>();

    if !expected_only.is_empty() {
        mismatches.push(format!(
            "  {label}: missing keys in bridge manifest: {}",
            expected_only.join(", ")
        ));
    }
    if !actual_only.is_empty() {
        mismatches.push(format!(
            "  {label}: extra keys in bridge manifest: {}",
            actual_only.join(", ")
        ));
    }
    if !changed.is_empty() {
        mismatches.push(format!(
            "  {label}: different values: {}",
            changed.join(", ")
        ));
    }
}

fn normalized_dependencies(manifest: &Value) -> Option<Value> {
    Some(Value::Table(normalize_dependencies(manifest_table(
        manifest,
        &["dependencies"],
    )?)))
}

fn normalized_target_dependencies(manifest: &Value) -> Option<Value> {
    let targets = manifest_table(manifest, &["target"])?;
    let mut normalized_targets = Map::new();

    for (target, target_value) in targets {
        let Some(target_table) = target_value.as_table() else {
            continue;
        };
        let Some(dependencies) = target_table.get("dependencies").and_then(Value::as_table) else {
            continue;
        };
        let mut normalized_target = Map::new();
        normalized_target.insert(
            "dependencies".to_owned(),
            Value::Table(normalize_dependencies(dependencies)),
        );
        normalized_targets.insert(target.clone(), Value::Table(normalized_target));
    }

    Some(Value::Table(normalized_targets))
}

fn normalize_dependencies(dependencies: &Table) -> Table {
    dependencies
        .iter()
        .map(|(name, value)| (name.clone(), normalize_dependency(value)))
        .collect()
}

fn normalize_dependency(value: &Value) -> Value {
    match value {
        Value::String(version) => {
            let mut dependency = Map::new();
            dependency.insert("version".to_owned(), Value::String(version.clone()));
            Value::Table(dependency)
        }
        Value::Table(dependency) => {
            let mut dependency = dependency.clone();
            if dependency
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.starts_with("../analyzed-ra"))
            {
                dependency.remove("path");
                dependency.insert(
                    "version".to_owned(),
                    Value::String(format!(
                        "={}",
                        env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by Cargo")
                    )),
                );
            }
            Value::Table(dependency)
        }
        _ => value.clone(),
    }
}

fn table_keys(table: &Table) -> Vec<String> {
    let mut keys = table.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn write_analyzed_root_module(root_rs: &Path, lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let analyzed_bridge = owned_source_path("analyzed_bridge.rs");
    let upstream_root = fs::read_to_string(lib_rs)?;
    let source = format!(
        r#"
#[path = {:?}]
pub mod analyzed_bridge;

{upstream_root}

pub use analyzed_bridge::{{
    RUST_ANALYZER_VERSION,
    PackageInstance, PackageInstanceKey, RustAnalyzerLspBoundary, RustAnalyzerPrivateBoundary,
    SessionOverlay, SessionOverlayCrate, SessionOverlayFile, SharedAnalyzerBackendKey,
    SharedAnalyzerCargoConfigKey, SharedAnalyzerConfig,
    SharedAnalyzerBackendSnapshot, SharedAnalyzerLoadKey,
    SharedAnalyzerProcMacroServerKey, SharedAnalyzerProvider, SharedAnalyzerRegistry,
    SharedAnalyzerSession, SharedAnalyzerWorldConfigKey, SharedAnalyzerWorldKey,
    SharedAnalyzerViewKey, SharedWorld, WorkspaceSummary, WorkspaceView,
    run_shared_rust_analyzer_lsp_session, run_shared_rust_analyzer_lsp_session_with_config,
    rust_analyzer_lsp_boundary,
    rust_analyzer_private_boundary, shared_analyzer_registry,
}};
"#,
        analyzed_bridge.to_string_lossy().into_owned()
    );
    fs::write(root_rs, source)?;
    println!("cargo:rerun-if-changed={}", analyzed_bridge.display());

    Ok(())
}

fn append_main_loop_session_module(main_loop_rs: &Path) -> Result<(), Box<dyn Error>> {
    let analyzed_session = owned_source_path("analyzed_session.rs");
    let mut file = fs::OpenOptions::new().append(true).open(main_loop_rs)?;
    writeln!(
        file,
        "\n#[path = {:?}]\npub(crate) mod analyzed_session;",
        analyzed_session.to_string_lossy().into_owned(),
    )?;
    println!("cargo:rerun-if-changed={}", analyzed_session.display());
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}

fn patch_config_source(config_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(config_rs)?;
    source = source.replace("ra_ap_rust_analyzer", "rust_analyzer");

    for guard in [
        "fn generate_package_json_config() {",
        "fn generate_config_documentation() {",
    ] {
        replace_once(
            &mut source,
            &format!("    #[test]\n    {guard}"),
            &format!(
                "    #[test]\n    #[ignore = \"regenerates files from the rust-analyzer source tree\"]\n    {guard}"
            ),
        )?;
    }

    fs::write(config_rs, source)?;
    Ok(())
}

fn patch_discover_source(discover_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(discover_rs)?;
    replace_once(
        &mut source,
        "    Buildfile(#[serde(serialize_with = \"serialize_abs_pathbuf\")] AbsPathBuf),\n",
        "",
    )?;
    fs::write(discover_rs, source)?;
    Ok(())
}

fn patch_global_state_source(global_state_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(global_state_rs)?;

    replace_once(&mut source, "    panic::AssertUnwindSafe,\n", "")?;
    replace_once(&mut source, "    ops::Not as _,\n", "")?;
    replace_once(
        &mut source,
        "use ide::{Analysis, AnalysisHost, Cancellable, FileId, SourceRootId};\n",
        "use ide::{Analysis, Cancellable, FileId, SourceRootId};\n",
    )?;
    replace_once(
        &mut source,
        "/// The most interesting components are `vfs`, which stores a consistent\n/// snapshot of the file systems, and `analysis_host`, which stores our\n/// incremental salsa database.\n",
        "/// The most interesting components are `vfs`, which stores a consistent\n/// snapshot of the file systems, and `analyzed_shared`, which stores our\n/// shared salsa database handle.\n",
    )?;
    replace_once(
        &mut source,
        "    base_db::{Crate, ProcMacroPaths, SourceDatabase, salsa::Revision},\n",
        "    base_db::{Crate, ProcMacroPaths},\n",
    )?;
    replace_once(&mut source, "use itertools::Itertools;\n", "")?;
    replace_once(
        &mut source,
        "use parking_lot::{\n    MappedRwLockReadGuard, Mutex, RwLock, RwLockReadGuard, RwLockUpgradableReadGuard,\n    RwLockWriteGuard,\n};\n",
        "use parking_lot::{Mutex, RwLock};\n",
    )?;
    replace_once(&mut source, "use stdx::thread;\n", "")?;
    replace_once(
        &mut source,
        "use tracing::{Level, span, trace};\n",
        "use tracing::{Level, span};\n",
    )?;
    replace_once(
        &mut source,
        "use vfs::{AbsPathBuf, AnchoredPathBuf, ChangeKind, Vfs, VfsPath};\n",
        "use vfs::{AbsPathBuf, AnchoredPathBuf, VfsPath};\n",
    )?;
    replace_once(
        &mut source,
        "    config::{Config, ConfigChange, ConfigErrors, RatomlFileKind},\n",
        "    config::{Config, ConfigErrors},\n",
    )?;
    replace_once(&mut source, "    reload,\n", "")?;

    replace_once(
        &mut source,
        "    pub(crate) analysis_host: AnalysisHost,\n    pub(crate) diagnostics: DiagnosticCollection,\n",
        "    pub(crate) analyzed_provider: crate::analyzed_bridge::SharedAnalyzerProvider,\n    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n    pub(crate) diagnostics: DiagnosticCollection,\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) analysis: Analysis,\n    pub(crate) check_fixes: CheckFixes,\n",
        "    pub(crate) analysis: Analysis,\n    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n    pub(crate) check_fixes: CheckFixes,\n",
    )?;
    replace_once(
        &mut source,
        "            analysis_host,\n            diagnostics: Default::default(),\n",
        "            analyzed_provider,\n            analyzed_shared,\n            diagnostics: Default::default(),\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) last_gc_revision: Revision,\n",
        "",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) cancellation_pool: thread::Pool,\n",
        "",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn new(sender: Sender<lsp_server::Message>, config: Config) -> GlobalState {\n",
        "    pub(crate) fn new(\n        sender: Sender<lsp_server::Message>,\n        config: Config,\n        analyzed_provider: crate::analyzed_bridge::SharedAnalyzerProvider,\n        analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n        analyzed_workspaces: Vec<ProjectWorkspace>,\n    ) -> GlobalState {\n",
    )?;
    replace_once(
        &mut source,
        "        let mut analysis_host = AnalysisHost::new(config.lru_parse_query_capacity());\n        if let Some(capacities) = config.lru_query_capacities_config() {\n            analysis_host.update_lru_capacities(capacities);\n        }\n        let (flycheck_sender, flycheck_receiver) = unbounded();\n",
        "        let (flycheck_sender, flycheck_receiver) = unbounded();\n",
    )?;
    replace_once(
        &mut source,
        "        let last_gc_revision = analysis_host.raw_database().nonce_and_revision().1;\n\n",
        "",
    )?;
    replace_once(
        &mut source,
        "        let cancellation_pool = thread::Pool::new(1);\n\n",
        "",
    )?;
    replace_once(&mut source, "            cancellation_pool,\n", "")?;
    replace_once(
        &mut source,
        "            workspaces: Arc::from(Vec::new()),\n",
        "            workspaces: Arc::new(analyzed_workspaces),\n",
    )?;
    replace_once(
        &mut source,
        "            minicore: MiniCoreRustAnalyzerInternalOnly::default(),\n            last_gc_revision,\n",
        "            minicore: MiniCoreRustAnalyzerInternalOnly::default(),\n",
    )?;
    replace_once(
        &mut source,
        "pub(crate) struct FetchWorkspaceResponse {\n    pub(crate) workspaces: Vec<anyhow::Result<ProjectWorkspace>>,\n    pub(crate) force_crate_graph_reload: bool,\n}\n",
        "#[derive(Debug)]\npub(crate) struct FetchWorkspaceResponse {\n    pub(crate) workspaces: Vec<anyhow::Result<ProjectWorkspace>>,\n    pub(crate) force_crate_graph_reload: bool,\n    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n}\n",
    )?;
    let process_changes_start = source
        .find("    pub(crate) fn process_changes(&mut self) -> bool {\n")
        .ok_or("could not find process_changes start")?;
    let snapshot_start = source[process_changes_start..]
        .find("    pub(crate) fn snapshot(&self) -> GlobalStateSnapshot {\n")
        .map(|index| process_changes_start + index)
        .ok_or("could not find snapshot start")?;
    source.replace_range(
        process_changes_start..snapshot_start,
        "    pub(crate) fn process_changes(&mut self) -> bool {\n        let _p = span!(Level::INFO, \"GlobalState::process_changes\").entered();\n        self.analyzed_process_shared_changes()\n    }\n\n",
    );
    replace_once(
        &mut source,
        "            workspaces: Arc::clone(&self.workspaces),\n            analysis: self.analysis_host.analysis(),\n            vfs: Arc::clone(&self.vfs),\n",
        "            workspaces: Arc::clone(&self.workspaces),\n            analysis: self.analyzed_shared.analysis(),\n            analyzed_shared: self.analyzed_shared.clone(),\n            vfs: Arc::clone(&self.vfs),\n",
    )?;
    replace_once(
        &mut source,
        "    vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,\n",
        "",
    )?;
    replace_once(&mut source, "            vfs: Arc::clone(&self.vfs),\n", "")?;
    replace_once(
        &mut source,
        "    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {\n        url_to_file_id(&self.vfs_read(), url)\n    }\n",
        "    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {\n        self.analyzed_shared.url_to_file_id(url)\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {\n        file_id_to_url(&self.vfs_read(), id)\n    }\n",
        "    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {\n        self.analyzed_shared\n            .file_id_to_url(id)\n            .expect(\"shared analyzer file id must have a url\")\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {\n        vfs_path_to_file_id(&self.vfs_read(), vfs_path)\n    }\n",
        "    pub(crate) fn vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {\n        self.analyzed_shared.vfs_path_to_file_id(vfs_path)\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {\n",
        "    pub(crate) fn base_vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {\n        self.analyzed_shared.base_vfs_path_to_file_id(vfs_path)\n    }\n\n    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {\n        let endings = self.vfs.read().1[&file_id];\n        let index = self.analysis.file_line_index(file_id)?;\n",
        "    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {\n        let endings = self\n            .analyzed_shared\n            .line_endings(file_id)\n            .expect(\"shared analyzer line endings must be tracked\");\n        let index = self.analysis.file_line_index(file_id)?;\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_version(&self, file_id: FileId) -> Option<i32> {\n        Some(self.mem_docs.get(self.vfs_read().file_path(file_id))?.version)\n    }\n",
        "    pub(crate) fn file_version(&self, file_id: FileId) -> Option<i32> {\n        let path = self.file_id_to_file_path(file_id);\n        Some(self.mem_docs.get(&path)?.version)\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn anchored_path(&self, path: &AnchoredPathBuf) -> Url {\n        let mut base = self.vfs_read().file_path(path.anchor).clone();\n",
        "    pub(crate) fn anchored_path(&self, path: &AnchoredPathBuf) -> Url {\n        let mut base = self.file_id_to_file_path(path.anchor);\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_id_to_file_path(&self, file_id: FileId) -> vfs::VfsPath {\n        self.vfs_read().file_path(file_id).clone()\n    }\n",
        "    pub(crate) fn file_id_to_file_path(&self, file_id: FileId) -> vfs::VfsPath {\n        self.analyzed_shared\n            .file_id_to_vfs_path(file_id)\n            .expect(\"shared analyzer file id must have a path\")\n    }\n",
    )?;
    replace_once(
        &mut source,
        "        let path = self.vfs_read().file_path(file_id).clone();\n        let path = path.as_path()?;\n",
        "        let path = self.file_id_to_file_path(file_id);\n        let path = path.as_path()?;\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_exists(&self, file_id: FileId) -> bool {\n        self.vfs.read().0.exists(file_id)\n    }\n",
        "    pub(crate) fn file_exists(&self, file_id: FileId) -> bool {\n        self.analyzed_shared.file_exists(file_id).unwrap_or(false)\n    }\n",
    )?;

    replace_once(
        &mut source,
        "impl Drop for GlobalState {\n    fn drop(&mut self) {\n        self.analysis_host.trigger_cancellation();\n    }\n}\n",
        "impl Drop for GlobalState {\n    fn drop(&mut self) {}\n}\n",
    )?;

    let enqueue_start = source
        .find("    fn enqueue_workspace_fetch(&mut self, path: AbsPathBuf, force_crate_graph_reload: bool) {\n")
        .ok_or("could not find enqueue_workspace_fetch start")?;
    let debounce_start = source[enqueue_start..]
        .find("    pub(crate) fn debounce_workspace_fetch(&mut self) {\n")
        .map(|index| enqueue_start + index)
        .ok_or("could not find debounce_workspace_fetch start")?;
    source.replace_range(enqueue_start..debounce_start, "");

    let vfs_read_start = source
        .find("    fn vfs_read(&self) -> MappedRwLockReadGuard<'_, vfs::Vfs> {\n")
        .ok_or("could not find vfs_read start")?;
    let snapshot_url_start = source[vfs_read_start..]
        .find("    /// Returns `None` if the file was excluded.\n    pub(crate) fn url_to_file_id")
        .map(|index| vfs_read_start + index)
        .ok_or("could not find snapshot url_to_file_id start")?;
    source.replace_range(vfs_read_start..snapshot_url_start, "");

    let free_url_start = source
        .find("pub(crate) fn file_id_to_url(vfs: &vfs::Vfs, id: FileId) -> Url {\n")
        .ok_or("could not find free file_id_to_url")?;
    source.truncate(free_url_start);

    fs::write(global_state_rs, source)?;
    Ok(())
}

fn patch_flycheck_to_proto_source(flycheck_to_proto_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(flycheck_to_proto_rs)?;

    replace_once(
        &mut source,
        "use vfs::{AbsPath, AbsPathBuf};\n",
        "use vfs::{AbsPath, AbsPathBuf, VfsPath};\n",
    )?;
    replace_once(
        &mut source,
        "    let uri = url_from_abs_path(&file_name);\n",
        "    let uri = snap\n        .base_vfs_path_to_file_id(&VfsPath::from(file_name.clone()))\n        .ok()\n        .flatten()\n        .map(|file_id| snap.file_id_to_url(file_id))\n        .unwrap_or_else(|| url_from_abs_path(&file_name));\n",
    )?;
    replace_once(
        &mut source,
        "        let state = GlobalState::new(\n            sender,\n            Config::new(\n                workspace_root.to_path_buf(),\n                ClientCapabilities::default(),\n                Vec::new(),\n                None,\n            ),\n        );\n",
        "        let ra_config = Config::new(\n            workspace_root.to_path_buf(),\n            ClientCapabilities::default(),\n            Vec::new(),\n            None,\n        );\n        let registry = crate::analyzed_bridge::shared_analyzer_registry();\n        let provider = crate::analyzed_bridge::SharedAnalyzerProvider::new(move |key, config, reload_path| {\n            registry.register(key, config, reload_path)\n        });\n        let (key, shared_config) = crate::analyzed_bridge::shared_analyzer_context_from_config(&ra_config).unwrap();\n        let session = provider.resolve(key, shared_config).unwrap();\n        let analyzed_shared = session.runtime();\n        let analyzed_workspaces = session.workspaces().unwrap();\n        let state = GlobalState::new(sender, ra_config, provider, analyzed_shared, analyzed_workspaces);\n",
    )?;

    fs::write(flycheck_to_proto_rs, source)?;
    Ok(())
}

fn patch_notification_source(notification_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(notification_rs)?;

    replace_once(
        &mut source,
        "use vfs::{AbsPathBuf, ChangeKind, VfsPath};\n",
        "use vfs::{AbsPathBuf, ChangeKind, VfsPath};\n",
    )?;
    replace_once(
        &mut source,
        "fn run_flycheck(state: &mut GlobalState, vfs_path: VfsPath) -> bool {\n",
        "pub(crate) fn run_flycheck(state: &mut GlobalState, vfs_path: VfsPath) -> bool {\n",
    )?;
    replace_once(
        &mut source,
        "    let file_id = state.vfs.read().0.file_id(&vfs_path);\n    if let Some((file_id, vfs::FileExcluded::No)) = file_id {\n",
        "    let base_file_id = state.analyzed_shared.base_vfs_path_to_file_id(&vfs_path);\n    let file_id = state.analyzed_shared.vfs_path_to_file_id(&vfs_path);\n    if let (Ok(Some(_)), Ok(Some(file_id))) = (base_file_id, file_id) {\n        let analyzed_vfs_path = vfs_path.clone();\n",
    )?;
    replace_once(
        &mut source,
        "        state.task_pool.handle.spawn_with_sender(stdx::thread::ThreadIntent::Worker, move |_| {\n            if let Err(e) = std::panic::catch_unwind(task) {\n                tracing::error!(\"flycheck task panicked: {e:?}\")\n            }\n        });\n        true\n",
        "        state.task_pool.handle.spawn_with_sender(stdx::thread::ThreadIntent::Worker, move |sender| {\n            match std::panic::catch_unwind(task) {\n                Ok(Ok(())) => {}\n                Ok(Err(_cancelled)) => {\n                    _ = sender.send(crate::main_loop::Task::AnalyzedRunFlycheck(analyzed_vfs_path));\n                }\n                Err(e) => tracing::error!(\"flycheck task panicked: {e:?}\"),\n            }\n        });\n        true\n",
    )?;

    fs::write(notification_rs, source)?;
    Ok(())
}

fn patch_dispatch_source(dispatch_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(dispatch_rs)?;

    replace_once(
        &mut source,
        "        let world = self.global_state.snapshot();\n",
        "        let world = self.global_state.snapshot();\n        let dispatched_edit_generation = world.analyzed_shared.edit_generation();\n        let analyzed_shared = world.analyzed_shared.clone();\n",
    )?;
    replace_once(
        &mut source,
        "            match thread_result_to_response::<R>(req.id.clone(), result) {\n                Ok(response) => Task::Response(response),\n                Err(_cancelled) if ALLOW_RETRYING => Task::Retry(req),\n                Err(_cancelled) => {\n                    let error = on_cancelled();\n                    Task::Response(Response { id: req.id, result: None, error: Some(error) })\n                }\n            }\n",
        "            match thread_result_to_response::<R>(req.id.clone(), result) {\n                Ok(response) => Task::Response(response),\n                Err(_cancelled) if ALLOW_RETRYING => Task::Retry(req),\n                Err(_cancelled)\n                    if analyzed_shared.edit_generation() == dispatched_edit_generation =>\n                {\n                    Task::Retry(req)\n                }\n                Err(_cancelled) => {\n                    let error = on_cancelled();\n                    Task::Response(Response { id: req.id, result: None, error: Some(error) })\n                }\n            }\n",
    )?;

    fs::write(dispatch_rs, source)?;
    Ok(())
}

fn patch_main_loop_source(main_loop_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(main_loop_rs)?;

    replace_once(
        &mut source,
        "    GlobalState::new(connection.sender, config).run(connection.receiver)\n",
        "    let _ = (config, connection);\n    anyhow::bail!(\"analyzed runs rust-analyzer through the shared daemon path\")\n",
    )?;
    replace_once(
        &mut source,
        "use ide_db::base_db::{SourceDatabase, VfsPath};\n",
        "use ide_db::base_db::VfsPath;\n",
    )?;
    replace_once(
        &mut source,
        "use lsp_types::{TextDocumentIdentifier, notification::Notification as _};\n",
        "use lsp_types::TextDocumentIdentifier;\n",
    )?;

    replace_once(
        &mut source,
        "    FetchWorkspace(ProjectWorkspaceProgress),\n    FetchBuildData(BuildDataProgress),\n",
        "    FetchWorkspace(ProjectWorkspaceProgress),\n    AnalyzedFetchWorkspace(FetchWorkspaceResponse),\n    AnalyzedRunFlycheck(VfsPath),\n",
    )?;
    replace_once(&mut source, "    LoadProcMacros(ProcMacroProgress),\n", "")?;
    replace_once(&mut source, "    Buildfile(AbsPathBuf),\n", "")?;

    replace_once(
        &mut source,
        "    global_state::{\n        FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState,\n        file_id_to_url, url_to_file_id,\n    },\n",
        "    global_state::{\n        FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState,\n    },\n",
    )?;
    replace_once(
        &mut source,
        "    global_state::{\n        FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState,\n    },\n",
        "    global_state::{FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState},\n",
    )?;
    replace_once(
        &mut source,
        "    reload::{BuildDataProgress, ProcMacroProgress, ProjectWorkspaceProgress},\n",
        "    reload::ProjectWorkspaceProgress,\n",
    )?;

    let run_start = source
        .find(
            "    fn run(mut self, inbox: Receiver<lsp_server::Message>) -> anyhow::Result<()> {\n",
        )
        .ok_or("could not find standalone GlobalState::run start")?;
    let register_start = source[run_start..]
        .find("    fn register_did_save_capability")
        .map(|index| run_start + index)
        .ok_or("could not find register_did_save_capability start")?;
    source.replace_range(run_start..register_start, "");
    replace_once(
        &mut source,
        "                        let resp = FetchWorkspaceResponse { workspaces, force_crate_graph_reload };\n",
        "                        let resp = FetchWorkspaceResponse {\n                            workspaces,\n                            force_crate_graph_reload,\n                            analyzed_shared: None,\n                        };\n",
    )?;
    replace_once(
        &mut source,
        "                            analyzed_shared: None,\n",
        "                            analyzed_shared: self.analyzed_shared.clone(),\n",
    )?;
    replace_once(
        &mut source,
        "            Task::DiscoverLinkedProjects(arg) => {\n",
        "            Task::AnalyzedFetchWorkspace(resp) => {\n                self.fetch_workspaces_queue.op_completed(resp);\n                if let Err(e) = self.fetch_workspace_error() {\n                    error!(\"FetchWorkspaceError: {e}\");\n                }\n                self.wants_to_switch = Some(\"fetched workspace\".to_owned());\n                self.diagnostics.clear_check_all();\n                self.report_progress(\"Fetching\", Progress::End, None, None, None);\n            }\n            Task::AnalyzedRunFlycheck(path) => {\n                crate::handlers::notification::run_flycheck(self, path);\n            }\n            Task::DiscoverLinkedProjects(arg) => {\n",
    )?;
    let fetch_workspace_start = source
        .find("            Task::FetchWorkspace(progress) => {\n")
        .ok_or("could not find FetchWorkspace task arm")?;
    let analyzed_fetch_start = source[fetch_workspace_start..]
        .find("            Task::AnalyzedFetchWorkspace(resp) => {\n")
        .map(|index| fetch_workspace_start + index)
        .ok_or("could not find AnalyzedFetchWorkspace task arm")?;
    source.replace_range(
        fetch_workspace_start..analyzed_fetch_start,
        "            Task::FetchWorkspace(ProjectWorkspaceProgress::Begin) => {\n                self.report_progress(\"Fetching\", Progress::Begin, None, None, None);\n            }\n",
    );
    let fetch_build_start = source
        .find("            Task::FetchBuildData(progress) => {\n")
        .ok_or("could not find FetchBuildData task arm")?;
    let build_deps_start = source[fetch_build_start..]
        .find("            Task::BuildDepsHaveChanged =>")
        .map(|index| fetch_build_start + index)
        .ok_or("could not find BuildDepsHaveChanged task arm")?;
    source.replace_range(fetch_build_start..build_deps_start, "");
    replace_once(
        &mut source,
        "                    let discover_path = match &arg {\n                        DiscoverProjectParam::Buildfile(it) => it,\n                        DiscoverProjectParam::Path(it) => it,\n                    };\n",
        "                    let DiscoverProjectParam::Path(discover_path) = &arg;\n",
    )?;
    replace_once(
        &mut source,
        "                    let arg = match arg {\n                        DiscoverProjectParam::Buildfile(it) => DiscoverArgument::Buildfile(it),\n                        DiscoverProjectParam::Path(it) => DiscoverArgument::Path(it),\n                    };\n",
        "                    let DiscoverProjectParam::Path(path) = arg;\n                    let arg = DiscoverArgument::Path(path);\n",
    )?;

    replace_once(
        &mut source,
        "                let uri = file_id_to_url(&self.vfs.read().0, file_id);\n",
        "                let Some(uri) = self.analyzed_shared.file_id_to_url(file_id) else {\n                    continue;\n                };\n",
    )?;
    replace_once(
        &mut source,
        "                    match url_to_file_id(&self.vfs.read().0, &diag.url) {\n",
        "                    match self.analyzed_base_url_to_file_id(&diag.url) {\n",
    )?;
    replace_once(
        &mut source,
        "                let diagnostics =
                    self.diagnostics.diagnostics_for(file_id).cloned().collect::<Vec<_>>();
                self.publish_diagnostics(uri, version, diagnostics);
",
        "                let diagnostics =
                    self.diagnostics.diagnostics_for(file_id).cloned().collect::<Vec<_>>();
                let diagnostics = self.analyzed_filter_diagnostics(diagnostics);
                self.publish_diagnostics(uri, version, diagnostics);
",
    )?;
    replace_once(
        &mut source,
        "        let db = self.analysis_host.raw_database();\n        let generation = self.diagnostics.next_generation();\n        let subscriptions = {\n            let vfs = &self.vfs.read().0;\n            self.mem_docs\n                .iter()\n                .map(|path| vfs.file_id(path).unwrap())\n                .filter_map(|(file_id, excluded)| {\n                    (excluded == vfs::FileExcluded::No).then_some(file_id)\n                })\n                .filter(|&file_id| {\n                    let source_root_id = db.file_source_root(file_id).source_root_id(db);\n                    let source_root = db.source_root(source_root_id).source_root(db);\n                    // Only publish diagnostics for files in the workspace, not from crates.io deps\n                    // or the sysroot.\n                    // While theoretically these should never have errors, we have quite a few false\n                    // positives particularly in the stdlib, and those diagnostics would stay around\n                    // forever if we emitted them here.\n                    !source_root.is_library\n                })\n                .collect::<std::sync::Arc<_>>()\n        };\n",
        "        let generation = self.diagnostics.next_generation();\n        let subscriptions = if let Some(shared) = &self.analyzed_shared {\n            let file_ids = self\n                .mem_docs\n                .iter()\n                .filter_map(|path| shared.vfs_path_to_file_id(path).ok().flatten())\n                .collect::<Vec<_>>();\n            let snap = self.snapshot();\n            file_ids\n                .into_iter()\n                .filter(|&file_id| {\n                    snap.analysis\n                        .is_library_file(file_id)\n                        .is_ok_and(|is_library| !is_library)\n                })\n                .collect::<std::sync::Arc<_>>()\n        } else {\n            let db = self.analysis_host.raw_database();\n            let vfs = &self.vfs.read().0;\n            self.mem_docs\n                .iter()\n                .map(|path| vfs.file_id(path).unwrap())\n                .filter_map(|(file_id, excluded)| {\n                    (excluded == vfs::FileExcluded::No).then_some(file_id)\n                })\n                .filter(|&file_id| {\n                    let source_root_id = db.file_source_root(file_id).source_root_id(db);\n                    let source_root = db.source_root(source_root_id).source_root(db);\n                    // Only publish diagnostics for files in the workspace, not from crates.io deps\n                    // or the sysroot.\n                    // While theoretically these should never have errors, we have quite a few false\n                    // positives particularly in the stdlib, and those diagnostics would stay around\n                    // forever if we emitted them here.\n                    !source_root.is_library\n                })\n                .collect::<std::sync::Arc<_>>()\n        };\n",
    )?;
    replace_once(
        &mut source,
        "                    sender\n                        .send(Task::Diagnostics(DiagnosticsTaskKind::Syntax(generation, diags)))\n                        .unwrap();\n",
        "                    let _ = sender\n                        .send(Task::Diagnostics(DiagnosticsTaskKind::Syntax(generation, diags)));\n",
    )?;
    replace_once(
        &mut source,
        "                        sender\n                            .send(Task::Diagnostics(DiagnosticsTaskKind::Semantic(\n                                generation, diags,\n                            )))\n                            .unwrap();\n",
        "                        let _ = sender.send(Task::Diagnostics(DiagnosticsTaskKind::Semantic(\n                            generation, diags,\n                        )));\n",
    )?;
    let diagnostics_start = source
        .find("        let generation = self.diagnostics.next_generation();\n        let subscriptions = if let Some(shared) = &self.analyzed_shared {\n")
        .ok_or("could not find shared diagnostics subscriptions")?;
    let diagnostics_end = source[diagnostics_start..]
        .find("\n        tracing::trace!(\"updating notifications for {:?}\", subscriptions);\n")
        .map(|index| diagnostics_start + index)
        .ok_or("could not find diagnostics subscription end")?;
    source.replace_range(
        diagnostics_start..diagnostics_end,
        "        let generation = self.diagnostics.next_generation();\n        let shared = &self.analyzed_shared;\n        let file_ids = self\n            .mem_docs\n            .iter()\n            .filter_map(|path| shared.vfs_path_to_file_id(path).ok().flatten())\n            .collect::<Vec<_>>();\n        let snap = self.snapshot();\n        let subscriptions = file_ids\n            .into_iter()\n            .filter(|&file_id| {\n                snap.analysis\n                    .is_library_file(file_id)\n                    .is_ok_and(|is_library| !is_library)\n            })\n            .collect::<std::sync::Arc<_>>();",
    );
    let update_tests_start = source
        .find("    fn update_tests(&mut self) {\n")
        .ok_or("could not find update_tests start")?;
    let update_status_start = source[update_tests_start..]
        .find("    fn update_status_or_notify(&mut self) {\n")
        .map(|index| update_tests_start + index)
        .ok_or("could not find update_status_or_notify start")?;
    source.replace_range(
        update_tests_start..update_status_start,
        "    fn update_tests(&mut self) {\n        if !self.vfs_done {\n            return;\n        }\n        let snapshot = self.snapshot();\n        let subscriptions = self\n            .mem_docs\n            .iter()\n            .filter_map(|path| self.analyzed_shared.vfs_path_to_file_id(path).ok().flatten())\n            .filter(|&file_id| {\n                snapshot\n                    .analysis\n                    .is_library_file(file_id)\n                    .is_ok_and(|is_library| !is_library)\n            })\n            .collect::<Vec<_>>();\n        tracing::trace!(\"updating tests for {:?}\", subscriptions);\n\n        self.task_pool.handle.spawn(ThreadIntent::LatencySensitive, {\n            move || {\n                let tests = subscriptions\n                    .iter()\n                    .copied()\n                    .filter_map(|f| snapshot.analysis.discover_tests_in_file(f).ok())\n                    .flatten()\n                    .collect::<Vec<_>>();\n\n                Task::DiscoverTest(lsp_ext::DiscoverTestResults {\n                    tests: tests\n                        .into_iter()\n                        .filter_map(|t| {\n                            let line_index = t.file.and_then(|f| snapshot.file_line_index(f).ok());\n                            to_proto::test_item(&snapshot, t, line_index.as_ref())\n                        })\n                        .collect(),\n                    scope: None,\n                    scope_file: Some(\n                        subscriptions\n                            .into_iter()\n                            .map(|f| TextDocumentIdentifier { uri: to_proto::url(&snapshot, f) })\n                            .collect(),\n                    ),\n                })\n            }\n        });\n    }\n\n",
    );
    replace_once(
        &mut source,
        "                            self.analysis_host.trigger_garbage_collection();\n",
        "                            crate::analyzed_bridge::shared_analyzer_registry().mark_gc_dirty();\n",
    )?;
    replace_once(
        &mut source,
        "            let current_revision = self.analysis_host.raw_database().nonce_and_revision().1;\n            // no work is currently being done, now we can block a bit and clean up our garbage\n            if self.task_pool.handle.is_empty()\n                && self.fmt_pool.handle.is_empty()\n                && current_revision != self.last_gc_revision\n            {\n                self.analysis_host.trigger_garbage_collection();\n                self.last_gc_revision = current_revision;\n            }\n",
        "            if self.task_pool.handle.is_empty() && self.fmt_pool.handle.is_empty() {\n                crate::analyzed_bridge::shared_analyzer_registry().mark_gc_dirty();\n            }\n",
    )?;
    fs::write(main_loop_rs, source)?;
    Ok(())
}

fn patch_reload_source(reload_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(reload_rs)?;

    replace_once(
        &mut source,
        "use hir::{ChangeWithProcMacros, ProcMacrosBuilder, db::DefDatabase};\n",
        "use hir::ChangeWithProcMacros;\n",
    )?;
    replace_once(
        &mut source,
        "    base_db::{CrateGraphBuilder, ProcMacroLoadingError, ProcMacroPaths, salsa::Durability},\n",
        "    base_db::{CrateGraphBuilder, ProcMacroPaths},\n",
    )?;
    replace_once(
        &mut source,
        "use load_cargo::{ProjectFolders, load_proc_macro};\n",
        "use load_cargo::ProjectFolders;\n",
    )?;
    replace_once(
        &mut source,
        "    config::{Config, FilesWatcher, LinkedProject},\n",
        "    config::{Config, FilesWatcher},\n",
    )?;
    replace_once(
        &mut source,
        "    main_loop::{DiscoverProjectParam, Task},\n",
        "    main_loop::Task,\n",
    )?;
    replace_once(
        &mut source,
        "    ManifestPath, ProjectWorkspace, ProjectWorkspaceKind, WorkspaceBuildScripts, project_json,\n",
        "    ManifestPath, ProjectWorkspace, ProjectWorkspaceKind, WorkspaceBuildScripts, project_json,\n",
    )?;
    replace_once(
        &mut source,
        "use tracing::{debug, info};\n",
        "use tracing::info;\n",
    )?;
    replace_once(
        &mut source,
        "pub(crate) enum ProjectWorkspaceProgress {\n    Begin,\n    Report(String),\n    End(Vec<anyhow::Result<ProjectWorkspace>>, bool),\n}\n\n#[derive(Debug)]\npub(crate) enum BuildDataProgress {\n    Begin,\n    Report(String),\n    End((Arc<Vec<ProjectWorkspace>>, Vec<anyhow::Result<WorkspaceBuildScripts>>)),\n}\n\n#[derive(Debug)]\npub(crate) enum ProcMacroProgress {\n    Begin,\n    Report(String),\n    End(ChangeWithProcMacros),\n}\n",
        "pub(crate) enum ProjectWorkspaceProgress {\n    Begin,\n}\n",
    )?;

    replace_once(
        &mut source,
        "        if self.config.lru_parse_query_capacity() != old_config.lru_parse_query_capacity() {\n            self.analysis_host.update_lru_capacity(self.config.lru_parse_query_capacity());\n        }\n        if self.config.lru_query_capacities_config() != old_config.lru_query_capacities_config() {\n            self.analysis_host.update_lru_capacities(\n                &self.config.lru_query_capacities_config().cloned().unwrap_or_default(),\n            );\n        }\n\n",
        "",
    )?;
    replace_once(
        &mut source,
        "        if self.analysis_host.raw_database().expand_proc_attr_macros()\n            != self.config.expand_proc_attr_macros()\n        {\n            self.analysis_host.raw_database_mut().set_expand_proc_attr_macros_with_durability(\n                self.config.expand_proc_attr_macros(),\n                Durability::HIGH,\n            );\n        }\n\n",
        "",
    )?;
    replace_once(
        &mut source,
        "        info!(%cause, \"will fetch workspaces\");\n\n        self.task_pool.handle.spawn_with_sender(ThreadIntent::Worker, {\n",
        "        info!(%cause, \"will fetch workspaces\");\n        let reload_path = path.clone();\n\n        let provider = self.analyzed_provider.clone();\n        let shared_context = crate::analyzed_bridge::shared_analyzer_context_from_config(&self.config);\n        let current_shared = self.analyzed_shared.clone();\n        self.task_pool.handle.spawn_with_sender(ThreadIntent::Worker, move |sender| {\n            sender.send(Task::FetchWorkspace(ProjectWorkspaceProgress::Begin)).unwrap();\n            let response = match shared_context {\n                Ok((key, config)) => provider\n                    .resolve_reloading(key, config, reload_path)\n                    .and_then(|session| {\n                        let analyzed_shared = session.runtime();\n                        let workspaces = session.workspaces()?;\n                        Ok(FetchWorkspaceResponse {\n                            workspaces: workspaces.into_iter().map(Ok).collect(),\n                            force_crate_graph_reload,\n                            analyzed_shared,\n                        })\n                    })\n                    .unwrap_or_else(|error| FetchWorkspaceResponse {\n                        workspaces: vec![Err(error)],\n                        force_crate_graph_reload,\n                        analyzed_shared: current_shared.clone(),\n                    }),\n                Err(error) => FetchWorkspaceResponse {\n                    workspaces: vec![Err(error)],\n                    force_crate_graph_reload,\n                    analyzed_shared: current_shared.clone(),\n                },\n            };\n            sender.send(Task::AnalyzedFetchWorkspace(response)).unwrap();\n        });\n        return;\n\n        self.task_pool.handle.spawn_with_sender(ThreadIntent::Worker, {\n",
    )?;
    let fallback_start = source
        .find("        return;\n\n        self.task_pool.handle.spawn_with_sender(ThreadIntent::Worker, {\n")
        .ok_or("could not find standalone workspace fetch fallback")?;
    let switch_start = source[fallback_start..]
        .find("    pub(crate) fn switch_workspaces")
        .map(|index| fallback_start + index)
        .ok_or("could not find switch_workspaces start")?;
    source.replace_range(fallback_start..switch_start, "    }\n\n");
    replace_once(
        &mut source,
        "        let Some(FetchWorkspaceResponse { workspaces, force_crate_graph_reload }) =\n            self.fetch_workspaces_queue.last_op_result()\n        else {\n            return;\n        };\n",
        "        let Some(FetchWorkspaceResponse {\n            workspaces,\n            force_crate_graph_reload,\n            analyzed_shared,\n        }) = self.fetch_workspaces_queue.last_op_result()\n        else {\n            return;\n        };\n        self.analyzed_shared = analyzed_shared.clone();\n",
    )?;
    replace_once(
        &mut source,
        "        self.local_roots_parent_map = Arc::new(self.source_root_config.source_root_parent_map());\n\n        info!(?cause, \"recreating the crate graph\");\n",
        "        self.local_roots_parent_map = Arc::new(self.source_root_config.source_root_parent_map());\n        self.analyzed_reload_config_from_shared();\n\n        info!(?cause, \"recreating the crate graph\");\n",
    )?;
    let recreate_start = source
        .find("    fn recreate_crate_graph(&mut self, cause: String, initial_build: bool) {\n")
        .ok_or("could not find recreate_crate_graph start")?;
    let finish_start = source[recreate_start..]
        .find("    pub(crate) fn finish_loading_crate_graph(&mut self) {\n")
        .map(|index| recreate_start + index)
        .ok_or("could not find finish_loading_crate_graph start")?;
    source.replace_range(
        recreate_start..finish_start,
        "    fn recreate_crate_graph(&mut self, cause: String, initial_build: bool) {\n        let _ = (cause, initial_build);\n        self.detached_files = self\n            .workspaces\n            .iter()\n            .filter_map(|ws| match &ws.kind {\n                ProjectWorkspaceKind::DetachedFile { file, .. } => Some(file.clone()),\n                _ => None,\n            })\n            .collect();\n        self.incomplete_crate_graph = false;\n        self.finish_loading_crate_graph();\n    }\n\n    pub(crate) fn fetch_build_data(&mut self, cause: Cause) {\n        let _ = cause;\n        let workspaces = Arc::new(self.workspaces.as_ref().clone());\n        let response = FetchBuildDataResponse {\n            build_scripts: workspaces\n                .iter()\n                .map(|_| Ok(WorkspaceBuildScripts::default()))\n                .collect(),\n            workspaces,\n        };\n        self.fetch_build_data_queue.op_completed(response);\n    }\n\n    pub(crate) fn fetch_proc_macros(\n        &mut self,\n        cause: Cause,\n        change: ChangeWithProcMacros,\n        paths: Vec<ProcMacroPaths>,\n    ) {\n        let _ = (cause, change, paths);\n        self.fetch_proc_macros_queue.op_completed(true);\n    }\n\n",
    );
    let eq_ignore_start = source
        .find("/// Similar to [`str::eq_ignore_ascii_case`] but instead of ignoring\n")
        .ok_or("could not find eq_ignore_underscore start")?;
    source.truncate(eq_ignore_start);
    fs::write(reload_rs, source)?;
    Ok(())
}

fn patch_test_tool_attributes(src_dir: &Path) -> Result<(), Box<dyn Error>> {
    for relative_path in ["cli/scip.rs", "lsp/to_proto.rs"] {
        let path = src_dir.join(relative_path);
        let source = fs::read_to_string(&path)?;
        let source = source
            .replace("#[ra_ap_rust_analyzer::rust_fixture] ", "")
            .replace("#[ra_ap_rust_analyzer::rust_fixture]", "")
            .replace("#[rust_analyzer::rust_fixture] ", "")
            .replace("#[rust_analyzer::rust_fixture]", "");
        fs::write(path, source)?;
    }
    Ok(())
}

fn patch_slow_tests(slow_tests: &Path) -> Result<(), Box<dyn Error>> {
    for name in [
        "main.rs",
        "ratoml.rs",
        "support.rs",
        "cli.rs",
        "flycheck.rs",
    ] {
        patch_slow_tests_imports(&slow_tests.join(name))?;
    }
    patch_slow_tests_support(&slow_tests.join("support.rs"))?;
    Ok(())
}

fn patch_slow_tests_imports(path: &Path) -> Result<(), Box<dyn Error>> {
    let source = fs::read_to_string(path)?;
    let mut source = source
        .replace("use rust_analyzer::", "use ra_ap_rust_analyzer::")
        .replace(" rust_analyzer::", " ra_ap_rust_analyzer::")
        .replace("<rust_analyzer::", "<ra_ap_rust_analyzer::")
        .replace(
            "use test_utils::skip_slow_tests;\n",
            "fn skip_slow_tests() -> bool {\n    (std::env::var(\"CI\").is_err() && std::env::var(\"RUN_SLOW_TESTS\").is_err())\n        || std::env::var(\"SKIP_SLOW_TESTS\").is_ok()\n}\n",
        )
        .replace(
            r#".replace("C:\\", "/c:/").replace('\\', "/")"#,
            ".analyzed_uri_path()",
        );
    if source.contains(".analyzed_uri_path()") {
        source.push_str(ANALYZED_URI_PATH_HELPER);
    }
    fs::write(path, source)?;
    Ok(())
}

// The upstream tests rewrite expected paths into URI form with a hardcoded
// C: drive; this generalizes the rewrite to whatever drive the test
// directory lives on.
const ANALYZED_URI_PATH_HELPER: &str = r#"
trait AnalyzedUriPath {
    fn analyzed_uri_path(self) -> String;
}

impl AnalyzedUriPath for String {
    fn analyzed_uri_path(self) -> String {
        let path = self.replace('\\', "/");
        let mut chars = path.chars();
        match (chars.next(), chars.next()) {
            (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
                format!("/{}:{}", drive.to_ascii_lowercase(), chars.as_str())
            }
            _ => path,
        }
    }
}
"#;

fn patch_slow_tests_support(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(path)?;
    replace_once(&mut source, "    lsp, main_loop,\n", "    lsp,\n")?;
    replace_once(
        &mut source,
        "            .spawn(move || main_loop(config, connection).unwrap())\n",
        "            .spawn(move || {\n                ra_ap_rust_analyzer::run_shared_rust_analyzer_lsp_session_with_config(\n                    config,\n                    connection,\n                )\n                .unwrap()\n            })\n",
    )?;
    fs::write(path, source)?;
    Ok(())
}

fn write_slow_tests_wrapper(slow_tests: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let main_rs = slow_tests.join("main.rs");
    let mut body = String::new();
    for line in fs::read_to_string(&main_rs)?.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//!")
            || matches!(
                trimmed,
                "#![allow(clippy::disallowed_types)]"
                    | "#![cfg_attr(feature = \"in-rust-tree\", feature(rustc_private))]"
            )
            || matches!(
                trimmed,
                "mod cli;" | "mod flycheck;" | "mod ratoml;" | "mod support;" | "mod testdir;"
            )
        {
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }

    let body_rs = slow_tests.join("analyzed-slow-tests-main.rs");
    fs::write(&body_rs, body)?;
    let wrapper_rs = slow_tests.join("analyzed-slow-tests.rs");
    fs::write(
        &wrapper_rs,
        format!(
            "#[path = {:?}]\nmod cli;\n#[path = {:?}]\nmod flycheck;\n#[path = {:?}]\nmod ratoml;\n#[path = {:?}]\nmod support;\n#[path = {:?}]\nmod testdir;\ninclude!({:?});\n",
            slow_tests.join("cli.rs").to_string_lossy().into_owned(),
            slow_tests
                .join("flycheck.rs")
                .to_string_lossy()
                .into_owned(),
            slow_tests.join("ratoml.rs").to_string_lossy().into_owned(),
            slow_tests.join("support.rs").to_string_lossy().into_owned(),
            slow_tests.join("testdir.rs").to_string_lossy().into_owned(),
            body_rs.to_string_lossy().into_owned(),
        ),
    )?;
    Ok(wrapper_rs)
}

fn replace_once(
    source: &mut String,
    needle: &str,
    replacement: &str,
) -> Result<(), Box<dyn Error>> {
    let Some(index) = source.find(needle) else {
        return Err(format!("could not find source fragment:\n{needle}").into());
    };
    source.replace_range(index..index + needle.len(), replacement);
    Ok(())
}
