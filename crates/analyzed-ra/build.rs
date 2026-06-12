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
    write_bridge_module(&generated_src.join("analyzed_bridge.rs"), &package.version)?;
    append_main_loop_session_module(&generated_src.join("main_loop.rs"))?;
    append_bridge_export(&generated_src.join("lib.rs"))?;
    let slow_tests = generated.join("tests/slow-tests");
    patch_slow_tests(&slow_tests)?;
    let slow_tests_wrapper = write_slow_tests_wrapper(&slow_tests)?;
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

fn append_bridge_export(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut file = fs::OpenOptions::new().append(true).open(lib_rs)?;
    file.write_all(
        br#"

	pub mod analyzed_bridge;
	pub use analyzed_bridge::{
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
	};
	"#,
    )?;

    Ok(())
}

fn append_main_loop_session_module(main_loop_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut file = fs::OpenOptions::new().append(true).open(main_loop_rs)?;
    file.write_all(MAIN_LOOP_SESSION_MODULE.as_bytes())?;
    Ok(())
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
        "    let file_id = state.vfs.read().0.file_id(&vfs_path);\n    if let Some((file_id, vfs::FileExcluded::No)) = file_id {\n",
        "    let file_id = state.analyzed_shared.base_vfs_path_to_file_id(&vfs_path);\n    if let Ok(Some(file_id)) = file_id {\n",
    )?;
    replace_once(
        &mut source,
        "    global_state::{FetchWorkspaceRequest, GlobalState},\n",
        "    global_state::{FetchWorkspaceRequest, GlobalState, GlobalStateSnapshot},\n",
    )?;
    replace_once(
        &mut source,
        "        let task: Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe> =\n            match invocation_strategy {\n",
        "        let task: Box<dyn Fn(&GlobalStateSnapshot) -> ide::Cancellable<()> + Send + UnwindSafe> =\n            match invocation_strategy {\n",
    )?;
    replace_once(
        &mut source,
        "                InvocationStrategy::Once => {\n                    Box::new(move || {\n",
        "                InvocationStrategy::Once => {\n                    Box::new(move |world: &GlobalStateSnapshot| {\n",
    )?;
    replace_once(
        &mut source,
        "                        let world = world;\n",
        "",
    )?;
    replace_once(
        &mut source,
        "                InvocationStrategy::PerWorkspace => {\n                    Box::new(move || {\n",
        "                InvocationStrategy::PerWorkspace => {\n                    Box::new(move |world: &GlobalStateSnapshot| {\n",
    )?;
    replace_once(
        &mut source,
        "                        let target = TargetSpec::for_file(&world, file_id)?.map(|it| {\n",
        "                        let target = TargetSpec::for_file(world, file_id)?.map(|it| {\n",
    )?;
    replace_once(
        &mut source,
        "        state.task_pool.handle.spawn_with_sender(stdx::thread::ThreadIntent::Worker, move |_| {\n            if let Err(e) = std::panic::catch_unwind(task) {\n                tracing::error!(\"flycheck task panicked: {e:?}\")\n            }\n        });\n        true\n",
        "        let analyzed_shared = state.analyzed_shared.clone();\n        state.task_pool.handle.spawn_with_sender(stdx::thread::ThreadIntent::Worker, move |_| {\n            let mut world = world;\n            loop {\n                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(&world))) {\n                    Ok(Ok(())) => break,\n                    Ok(Err(_cancelled)) => world.analysis = analyzed_shared.analysis(),\n                    Err(e) => {\n                        tracing::error!(\"flycheck task panicked: {e:?}\");\n                        break;\n                    }\n                }\n            }\n        });\n        true\n",
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
        "            match thread_result_to_response::<R>(req.id.clone(), result) {\n                Ok(response) => Task::Response(response),\n                Err(_cancelled) if ALLOW_RETRYING => Task::Retry(req),\n                Err(_cancelled)\n                    if analyzed_shared.edit_generation() == dispatched_edit_generation =>\n                {\n                    // The cancellation came from another session's write to the shared\n                    // world; this session's inputs are unchanged, so the request is\n                    // still meaningful and can be retried.\n                    Task::Retry(req)\n                }\n                Err(_cancelled) => {\n                    let error = on_cancelled();\n                    Task::Response(Response { id: req.id, result: None, error: Some(error) })\n                }\n            }\n",
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
        "    FetchWorkspace(ProjectWorkspaceProgress),\n    AnalyzedFetchWorkspace(FetchWorkspaceResponse),\n",
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
        "            Task::AnalyzedFetchWorkspace(resp) => {\n                self.fetch_workspaces_queue.op_completed(resp);\n                if let Err(e) = self.fetch_workspace_error() {\n                    error!(\"FetchWorkspaceError: {e}\");\n                }\n                self.wants_to_switch = Some(\"fetched workspace\".to_owned());\n                self.diagnostics.clear_check_all();\n                self.report_progress(\"Fetching\", Progress::End, None, None, None);\n            }\n            Task::DiscoverLinkedProjects(arg) => {\n",
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
    let source = source
        .replace("use rust_analyzer::", "use ra_ap_rust_analyzer::")
        .replace(" rust_analyzer::", " ra_ap_rust_analyzer::")
        .replace("<rust_analyzer::", "<ra_ap_rust_analyzer::")
        .replace(
            "use test_utils::skip_slow_tests;\n",
            "fn skip_slow_tests() -> bool {\n    (std::env::var(\"CI\").is_err() && std::env::var(\"RUN_SLOW_TESTS\").is_err())\n        || std::env::var(\"SKIP_SLOW_TESTS\").is_ok()\n}\n",
        );
    fs::write(path, source)?;
    Ok(())
}

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

fn write_bridge_module(path: &Path, rust_analyzer_version: &str) -> Result<(), Box<dyn Error>> {
    fs::write(
        path,
        BRIDGE_MODULE.replace("__ANALYZED_RA_VERSION__", rust_analyzer_version),
    )?;
    Ok(())
}

const BRIDGE_MODULE: &str = r#"
	use std::{
	    collections::{BTreeMap, BTreeSet, btree_map::Entry},
	    env,
	    path::{Path, PathBuf},
	    sync::{
	        Arc, Condvar, Mutex, OnceLock, Weak,
	        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
	    },
	    thread,
	    time::Duration,
	};

	use hir::{ChangeWithProcMacros, ProcMacrosBuilder, collect_ty_garbage};
	use ide::{Analysis, AnalysisHost, FileId, RootDatabase};
	use ide_db::{
	    FxHashMap,
	    base_db::{
	        CrateGraphBuilder, DependencyBuilder, FileSet, LibraryRoots, LocalRoots,
	        ProcMacroPaths, SourceDatabase, SourceRoot, SourceRootId, all_crates,
	        salsa::{Database, Durability, Setter as _},
	    },
	};
	use load_cargo::{
	    AnalyzedProcMacroLoad, AnalyzedWorkspaceLoad, LoadCargoConfig, ProcMacroServerChoice,
	    analyzed_load_workspace_change,
	};
	use lsp_types::Url;
	use proc_macro_api::ProcMacroClient;
	use project_model::{CargoConfig, ManifestPath, ProjectWorkspace};
	use serde::Serialize;
	use vfs::{AbsPathBuf, Vfs, VfsPath};

const RUST_ANALYZER_VERSION: &str = "__ANALYZED_RA_VERSION__";

#[derive(Clone, Debug, Serialize)]
pub struct RustAnalyzerLspBoundary {
    pub main_loop: &'static str,
}

pub fn rust_analyzer_lsp_boundary() -> RustAnalyzerLspBoundary {
    let _main_loop = crate::main_loop;

    RustAnalyzerLspBoundary {
        main_loop: "ra_ap_rust_analyzer::main_loop",
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RustAnalyzerPrivateBoundary {
    pub global_state: &'static str,
    pub request_dispatcher: &'static str,
    pub notification_dispatcher: &'static str,
}

pub fn rust_analyzer_private_boundary() -> RustAnalyzerPrivateBoundary {
    let _global_state_size = std::mem::size_of::<crate::global_state::GlobalState>();
    let _request_dispatcher_size =
        std::mem::size_of::<crate::handlers::dispatch::RequestDispatcher<'_>>();
    let _notification_dispatcher_size =
        std::mem::size_of::<crate::handlers::dispatch::NotificationDispatcher<'_>>();

    RustAnalyzerPrivateBoundary {
        global_state: std::any::type_name::<crate::global_state::GlobalState>(),
        request_dispatcher: std::any::type_name::<
            crate::handlers::dispatch::RequestDispatcher<'_>,
        >(),
        notification_dispatcher: std::any::type_name::<
            crate::handlers::dispatch::NotificationDispatcher<'_>,
        >(),
    }
}

pub fn run_shared_rust_analyzer_lsp_session(
    connection: lsp_server::Connection,
    provider: SharedAnalyzerProvider,
) -> anyhow::Result<()> {
    crate::main_loop::analyzed_session::run_shared_lsp_session(connection, provider)
}

pub fn run_shared_rust_analyzer_lsp_session_with_config(
    config: crate::config::Config,
    connection: lsp_server::Connection,
) -> anyhow::Result<()> {
    let registry = shared_analyzer_registry();
    let provider = SharedAnalyzerProvider::new(move |key, config, reload_path| {
        registry.register(key, config, reload_path)
    });

    crate::main_loop::analyzed_session::run_shared_lsp_session_with_config(
        config,
        connection,
        provider,
    )
}

#[derive(Clone)]
pub struct SharedAnalyzerProvider {
    resolve: Arc<
        dyn Fn(
                SharedAnalyzerBackendKey,
                Arc<SharedAnalyzerConfig>,
                Option<AbsPathBuf>,
            ) -> anyhow::Result<SharedAnalyzerSession>
            + Send
            + Sync
            + std::panic::RefUnwindSafe,
    >,
}

impl SharedAnalyzerProvider {
    pub fn new<F>(resolve: F) -> Self
    where
        F: Fn(
                SharedAnalyzerBackendKey,
                Arc<SharedAnalyzerConfig>,
                Option<AbsPathBuf>,
            ) -> anyhow::Result<SharedAnalyzerSession>
            + Send
            + Sync
            + std::panic::RefUnwindSafe
            + 'static,
    {
        Self { resolve: Arc::new(resolve) }
    }

    pub(crate) fn resolve(
        &self,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
    ) -> anyhow::Result<SharedAnalyzerSession> {
        (self.resolve)(key, config, None)
    }

    pub(crate) fn resolve_reloading(
        &self,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload_path: Option<AbsPathBuf>,
    ) -> anyhow::Result<SharedAnalyzerSession> {
        (self.resolve)(key, config, reload_path)
    }
}

pub fn shared_analyzer_registry() -> Arc<SharedAnalyzerRegistry> {
    static REGISTRY: OnceLock<Arc<SharedAnalyzerRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(SharedAnalyzerRegistry::new())))
}

#[derive(Debug, Serialize)]
pub struct SharedAnalyzerBackendSnapshot {
    pub key: SharedAnalyzerBackendKey,
    pub client_sessions: usize,
    pub overlay_sessions: usize,
    pub overlay_files: usize,
    pub workspace_loads: Vec<WorkspaceSummary>,
}

pub struct SharedAnalyzerRegistry {
    state: Mutex<SharedAnalyzerRegistryState>,
    gc: SharedAnalyzerGcCoordinator,
}

#[derive(Default)]
struct SharedAnalyzerRegistryState {
    worlds: BTreeMap<SharedAnalyzerWorldKey, SharedAnalyzerWorldEntry>,
    views: BTreeMap<SharedAnalyzerBackendKey, SharedAnalyzerViewEntry>,
    loads: BTreeMap<SharedAnalyzerWorkspaceLoadKey, Arc<SharedAnalyzerWorkspaceLoad>>,
}

struct SharedAnalyzerWorldEntry {
    client_sessions: usize,
    world: Arc<Mutex<SharedWorld>>,
}

struct SharedAnalyzerViewEntry {
    client_sessions: usize,
    view: WorkspaceView,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SharedAnalyzerWorkspaceLoadKey {
    world: SharedAnalyzerWorldKey,
    project: String,
}

struct SharedAnalyzerWorkspaceLoad {
    result: Mutex<Option<Result<usize, String>>>,
    ready: Condvar,
}

struct SharedAnalyzerRegistryLease {
    registry: Weak<SharedAnalyzerRegistry>,
    key: SharedAnalyzerBackendKey,
}

struct SharedAnalyzerGcCoordinator {
    dirty: Arc<AtomicBool>,
}

impl SharedAnalyzerRegistry {
    fn new() -> Self {
        let gc = SharedAnalyzerGcCoordinator::spawn();

        Self {
            state: Mutex::new(SharedAnalyzerRegistryState::default()),
            gc,
        }
    }

    fn state(
        &self,
    ) -> anyhow::Result<std::sync::MutexGuard<'_, SharedAnalyzerRegistryState>> {
        self.state
            .lock()
            .map_err(|error| anyhow::format_err!("shared analyzer registry mutex is poisoned: {error}"))
    }

    pub fn register(
        self: &Arc<Self>,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload_path: Option<AbsPathBuf>,
    ) -> anyhow::Result<SharedAnalyzerSession> {
        let world = self.world(&key.shared_world)?;
        self.retain_world(&key.shared_world)?;
        let reload = reload_path.is_some();

        let result = (|| {
            let mut workspaces = Vec::new();

            for project in config.projects() {
                workspaces.push(self.ensure_workspace_loaded(
                    key.shared_world.clone(),
                    Arc::clone(&world),
                    shared_project_key(project),
                    SharedAnalyzerWorkspaceLoadSource::Project(project.clone()),
                    &config,
                    reload,
                )?);
            }
            for file in config.detached_files() {
                workspaces.push(self.ensure_workspace_loaded(
                    key.shared_world.clone(),
                    Arc::clone(&world),
                    shared_detached_file_key(file),
                    SharedAnalyzerWorkspaceLoadSource::DetachedFile(file.clone()),
                    &config,
                    reload,
                )?);
            }

            let view = WorkspaceView::new(workspaces, config.excluded_paths().to_vec());
            {
                let mut state = self.state()?;
                match state.views.entry(key.clone()) {
                    Entry::Occupied(mut entry) => {
                        entry.get_mut().client_sessions += 1;
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(SharedAnalyzerViewEntry {
                            client_sessions: 1,
                            view: view.clone(),
                        });
                    }
                }
            }

            Ok(SharedAnalyzerSession::new_registered(
                world,
                view,
                Arc::downgrade(self),
                key.clone(),
            ))
        })();

        if result.is_err() {
            self.release_world(&key.shared_world);
        }

        result
    }

    fn retain_world(&self, key: &SharedAnalyzerWorldKey) -> anyhow::Result<()> {
        let mut state = self.state()?;
        state
            .worlds
            .get_mut(key)
            .expect("shared world was registered")
            .client_sessions += 1;

        Ok(())
    }

    fn release_world(&self, key: &SharedAnalyzerWorldKey) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        release_world_state(&mut state, key);
    }

    fn world(
        &self,
        key: &SharedAnalyzerWorldKey,
    ) -> anyhow::Result<Arc<Mutex<SharedWorld>>> {
        let mut state = self.state()?;
        let entry = match state.worlds.entry(key.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(SharedAnalyzerWorldEntry {
                client_sessions: 0,
                world: Arc::new(Mutex::new(SharedWorld::new())),
            }),
        };

        Ok(Arc::clone(&entry.world))
    }

    fn ensure_workspace_loaded(
        &self,
        world_key: SharedAnalyzerWorldKey,
        world: Arc<Mutex<SharedWorld>>,
        load_key: String,
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
        reload: bool,
    ) -> anyhow::Result<usize> {
        if !reload
            && let Some(index) = world
                .lock()
                .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
                .workspace_index(&load_key)
        {
            return Ok(index);
        }

        let registry_load_key = SharedAnalyzerWorkspaceLoadKey {
            world: world_key,
            project: load_key,
        };
        let (load, leader) = {
            let mut state = self.state()?;
            match state.loads.entry(registry_load_key.clone()) {
                Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
                Entry::Vacant(entry) => {
                    let load = Arc::new(SharedAnalyzerWorkspaceLoad::new());
                    entry.insert(Arc::clone(&load));
                    (load, true)
                }
            }
        };

        if leader {
            let result = SharedWorld::prepare_workspace_load(source, config)
                .and_then(|loaded| {
                    world
                        .lock()
                        .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
                        .commit_workspace(loaded, reload)
                });
            load.finish(result);
            self.state()?.loads.remove(&registry_load_key);
        }

        load.wait()
    }

    pub fn unregister(&self, key: &SharedAnalyzerBackendKey) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        if let Some(entry) = state.views.get_mut(key) {
            entry.client_sessions = entry.client_sessions.saturating_sub(1);
            if entry.client_sessions == 0 {
                state.views.remove(key);
            }
        }

        release_world_state(&mut state, &key.shared_world);
    }

    pub fn backend_snapshots(&self) -> Vec<SharedAnalyzerBackendSnapshot> {
        let entries = {
            let Ok(state) = self.state.lock() else {
                return Vec::new();
            };
            state
                .views
                .iter()
                .filter_map(|(key, entry)| {
                    let world = state.worlds.get(&key.shared_world)?;
                    Some((
                        key.clone(),
                        entry.client_sessions,
                        Arc::clone(&world.world),
                        entry.view.clone(),
                    ))
                })
                .collect::<Vec<_>>()
        };

        entries
            .into_iter()
            .map(|(key, client_sessions, world, view)| {
                let (overlay_sessions, overlay_files, workspace_loads) = world
                    .lock()
                    .map(|world| {
                        (
                            world.active_overlay_sessions(),
                            world.overlay_files(),
                            world.workspace_summaries(&view),
                        )
                    })
                    .unwrap_or_default();

                SharedAnalyzerBackendSnapshot {
                    key,
                    client_sessions,
                    overlay_sessions,
                    overlay_files,
                    workspace_loads,
                }
            })
            .collect()
    }

    pub fn workspace_loads(&self) -> Vec<WorkspaceSummary> {
        self.backend_snapshots()
            .into_iter()
            .flat_map(|snapshot| snapshot.workspace_loads)
            .collect()
    }

    pub(crate) fn mark_gc_dirty(&self) {
        self.gc.mark_dirty();
    }
}

fn release_world_state(
    state: &mut SharedAnalyzerRegistryState,
    key: &SharedAnalyzerWorldKey,
) {
    if let Some(entry) = state.worlds.get_mut(key) {
        entry.client_sessions = entry.client_sessions.saturating_sub(1);
        if entry.client_sessions == 0 {
            state.worlds.remove(key);
        }
    }
}

impl SharedAnalyzerWorkspaceLoad {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn finish(&self, result: anyhow::Result<usize>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(result.map_err(|error| format!("{error:#}")));
            self.ready.notify_all();
        }
    }

    fn wait(&self) -> anyhow::Result<usize> {
        let mut slot = self
            .result
            .lock()
            .map_err(|error| anyhow::format_err!("shared analyzer load mutex is poisoned: {error}"))?;

        loop {
            if let Some(result) = &*slot {
                return result
                    .as_ref()
                    .copied()
                    .map_err(|error| anyhow::format_err!("{error}"));
            }
            slot = self
                .ready
                .wait(slot)
                .map_err(|error| anyhow::format_err!("shared analyzer load mutex is poisoned: {error}"))?;
        }
    }
}

impl SharedAnalyzerGcCoordinator {
    fn spawn() -> Self {
        let dirty = Arc::new(AtomicBool::new(false));
        let worker_dirty = Arc::clone(&dirty);

        thread::Builder::new()
            .name("shared analyzer gc".to_owned())
            .spawn(move || loop {
                if !worker_dirty.swap(false, Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }

                thread::sleep(Duration::from_millis(200));
                let registry = shared_analyzer_registry();
                let worlds = {
                    let Ok(state) = registry.state.lock() else {
                        continue;
                    };
                    state
                        .worlds
                        .values()
                        .map(|entry| Arc::clone(&entry.world))
                        .collect::<Vec<_>>()
                };

                let mut guards = Vec::new();
                let mut blocked = false;
                for world in &worlds {
                    match world.lock() {
                        Ok(world) => {
                            if world.any_session_busy() {
                                blocked = true;
                                break;
                            }
                            guards.push(world);
                        }
                        Err(error) => {
                            tracing::error!("shared world mutex is poisoned during gc: {error}");
                            blocked = true;
                            break;
                        }
                    }
                }

                if blocked {
                    drop(guards);
                    worker_dirty.store(true, Ordering::SeqCst);
                    continue;
                }

                if !guards.is_empty() {
                    for world in &mut guards {
                        world.synthetic_write();
                    }
                    unsafe { collect_ty_garbage() };
                }
            })
            .expect("failed to spawn shared analyzer gc thread");

        Self { dirty }
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerBackendKey {
    pub shared_world: SharedAnalyzerWorldKey,
    pub workspace_view: SharedAnalyzerViewKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerWorldKey {
    pub rust_analyzer_version: String,
    pub toolchain: Option<String>,
    pub sysroot: Option<String>,
    pub cargo_target: Option<String>,
    pub config: SharedAnalyzerWorldConfigKey,
    pub load: SharedAnalyzerLoadKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerWorldConfigKey {
    pub cargo: SharedAnalyzerCargoConfigKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerCargoConfigKey {
    pub all_targets: bool,
    pub features: String,
    pub target: Option<String>,
    pub sysroot: Option<String>,
    pub sysroot_src: Option<String>,
    pub rustc_source: Option<String>,
    pub extra_includes: Vec<String>,
    pub cfg_overrides: String,
    pub wrap_rustc_in_build_scripts: bool,
    pub invocation_strategy: String,
    pub run_build_script_command: String,
    pub extra_args: Vec<String>,
    pub extra_env: Vec<(String, Option<String>)>,
    pub target_dir_config: String,
    pub set_test: bool,
    pub no_deps: bool,
    pub metadata_extra_args: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerLoadKey {
    pub load_out_dirs_from_check: bool,
    pub proc_macro_server: SharedAnalyzerProcMacroServerKey,
    pub prefill_caches: bool,
    pub num_worker_threads: u16,
    pub proc_macro_processes: u16,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum SharedAnalyzerProcMacroServerKey {
    None,
    Sysroot,
    Explicit(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerViewKey {
    pub workspace_roots: Vec<String>,
    pub projects: Vec<String>,
    pub excluded_paths: Vec<String>,
    pub analysis: SharedAnalyzerAnalysisKey,
}

#[derive(Clone)]
enum SharedAnalyzerWorkspaceLoadSource {
    Project(crate::config::LinkedProject),
    DetachedFile(ManifestPath),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerAnalysisKey {
    pub initialization_options: Option<String>,
    pub workspace_configuration: Option<String>,
}

pub(crate) fn shared_analyzer_context_from_config(
    config: &crate::config::Config,
) -> anyhow::Result<(SharedAnalyzerBackendKey, Arc<SharedAnalyzerConfig>)> {
    let mut workspace_roots = config
        .workspace_roots()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    workspace_roots.sort();
    workspace_roots.dedup();
    let mut excluded_paths = config
        .excluded()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    excluded_paths.sort();
    excluded_paths.dedup();
    let projects = config.linked_or_discovered_projects();
    let detached_files = config
        .detached_files()
        .iter()
        .cloned()
        .map(ManifestPath::try_from)
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let mut project_keys = projects.iter().map(shared_project_key).collect::<Vec<_>>();
    project_keys.extend(detached_files.iter().map(shared_detached_file_key));
    project_keys.sort();
    project_keys.dedup();
    let cargo_config = config.cargo(None);
    let load = shared_load_config_from_config(config)?;
    let analysis = SharedAnalyzerAnalysisKey {
        initialization_options: None,
        workspace_configuration: None,
    };
    let backend_key = SharedAnalyzerBackendKey {
        shared_world: SharedAnalyzerWorldKey {
            rust_analyzer_version: RUST_ANALYZER_VERSION.to_owned(),
            toolchain: env::var("RUSTUP_TOOLCHAIN").ok(),
            sysroot: env::var("RUST_SRC_PATH").ok(),
            cargo_target: cargo_config
                .target
                .clone()
                .or_else(|| env::var("CARGO_BUILD_TARGET").ok()),
            config: SharedAnalyzerWorldConfigKey {
                cargo: cargo_config_key(&cargo_config),
            },
            load: load.key.clone(),
        },
        workspace_view: SharedAnalyzerViewKey {
            workspace_roots: workspace_roots.clone(),
            projects: project_keys,
            excluded_paths: excluded_paths.clone(),
            analysis,
        },
    };

    Ok((
        backend_key,
        Arc::new(SharedAnalyzerConfig {
            workspace_roots,
            excluded_paths,
            projects,
            detached_files,
            cargo_config,
            load,
        }),
    ))
}

pub struct SharedAnalyzerConfig {
    workspace_roots: Vec<String>,
    excluded_paths: Vec<String>,
    projects: Vec<crate::config::LinkedProject>,
    detached_files: Vec<ManifestPath>,
    pub(crate) cargo_config: CargoConfig,
    pub(crate) load: SharedLoadConfig,
}

impl SharedAnalyzerConfig {
    pub fn workspace_roots(&self) -> &[String] {
        &self.workspace_roots
    }

    pub fn excluded_paths(&self) -> &[String] {
        &self.excluded_paths
    }

    fn projects(&self) -> &[crate::config::LinkedProject] {
        &self.projects
    }

    fn detached_files(&self) -> &[ManifestPath] {
        &self.detached_files
    }
}

fn shared_project_key(project: &crate::config::LinkedProject) -> String {
    match project {
        crate::config::LinkedProject::ProjectManifest(manifest) => format!("manifest:{manifest}"),
        crate::config::LinkedProject::InlineProjectJson(project) => format!("json:{project:?}"),
    }
}

fn shared_detached_file_key(file: &ManifestPath) -> String {
    format!("detached:{file}")
}

#[derive(Clone)]
pub(crate) struct SharedLoadConfig {
    key: SharedAnalyzerLoadKey,
}

impl SharedLoadConfig {
    pub(crate) fn to_load_cargo_config(&self) -> LoadCargoConfig {
        LoadCargoConfig {
            load_out_dirs_from_check: self.key.load_out_dirs_from_check,
            with_proc_macro_server: match &self.key.proc_macro_server {
                SharedAnalyzerProcMacroServerKey::None => ProcMacroServerChoice::None,
                SharedAnalyzerProcMacroServerKey::Sysroot => ProcMacroServerChoice::Sysroot,
                SharedAnalyzerProcMacroServerKey::Explicit(path) => {
                    ProcMacroServerChoice::Explicit(AbsPathBuf::assert_utf8(PathBuf::from(path)))
                }
            },
            prefill_caches: self.key.prefill_caches,
            num_worker_threads: self.key.num_worker_threads as usize,
            proc_macro_processes: self.key.proc_macro_processes as usize,
        }
    }
}

fn shared_load_config_from_config(
    config: &crate::config::Config,
) -> anyhow::Result<SharedLoadConfig> {
    Ok(SharedLoadConfig {
        key: SharedAnalyzerLoadKey {
            load_out_dirs_from_check: config.run_build_scripts(None),
            proc_macro_server: if config.expand_proc_macros() {
                config
                    .proc_macro_srv()
                    .map(|path| SharedAnalyzerProcMacroServerKey::Explicit(path.to_string()))
                    .unwrap_or(SharedAnalyzerProcMacroServerKey::Sysroot)
            } else {
                SharedAnalyzerProcMacroServerKey::None
            },
            prefill_caches: config.prefill_caches(),
            num_worker_threads: u16::try_from(config.prime_caches_num_threads())?,
            proc_macro_processes: u16::try_from(config.proc_macro_num_processes())?,
        },
    })
}

fn cargo_config_key(config: &CargoConfig) -> SharedAnalyzerCargoConfigKey {
    let mut extra_env = config
        .extra_env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    extra_env.sort();

    SharedAnalyzerCargoConfigKey {
        all_targets: config.all_targets,
        features: format!("{:?}", config.features),
        target: config.target.clone(),
        sysroot: config.sysroot.as_ref().map(|it| format!("{it:?}")),
        sysroot_src: config.sysroot_src.as_ref().map(ToString::to_string),
        rustc_source: config.rustc_source.as_ref().map(|it| format!("{it:?}")),
        extra_includes: config
            .extra_includes
            .iter()
            .map(ToString::to_string)
            .collect(),
        cfg_overrides: format!("{:?}", config.cfg_overrides),
        wrap_rustc_in_build_scripts: config.wrap_rustc_in_build_scripts,
        invocation_strategy: format!("{:?}", config.invocation_strategy),
        run_build_script_command: format!("{:?}", config.run_build_script_command),
        extra_args: config.extra_args.clone(),
        extra_env,
        target_dir_config: format!("{:?}", config.target_dir_config),
        set_test: config.set_test,
        no_deps: config.no_deps,
        metadata_extra_args: config.metadata_extra_args.clone(),
    }
}

pub(crate) fn patch_path_prefix(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    if cfg!(windows) {
        let mut components = path.components();
        match components.next() {
            Some(Component::Prefix(prefix)) => {
                let prefix = match prefix.kind() {
                    Prefix::Disk(disk) => format!("{}:", disk.to_ascii_uppercase() as char),
                    Prefix::VerbatimDisk(disk) => {
                        format!(r"\\?\{}:", disk.to_ascii_uppercase() as char)
                    }
                    _ => return path,
                };
                let mut path = PathBuf::new();
                path.push(prefix);
                path.extend(components);
                path
            }
            _ => path,
        }
    } else {
        path
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSummary {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
    pub files: usize,
    pub proc_macro_server: bool,
}

#[derive(Clone)]
pub struct SharedAnalyzerSession {
    world: Arc<Mutex<SharedWorld>>,
    view: WorkspaceView,
    runtime: SharedAnalyzerRuntime,
}

impl SharedAnalyzerSession {
    pub fn new(world: Arc<Mutex<SharedWorld>>, view: WorkspaceView) -> Self {
        let runtime = SharedAnalyzerRuntime::new(Arc::clone(&world), &view);

        Self {
            world,
            view,
            runtime,
        }
    }

    fn new_registered(
        world: Arc<Mutex<SharedWorld>>,
        view: WorkspaceView,
        registry: Weak<SharedAnalyzerRegistry>,
        key: SharedAnalyzerBackendKey,
    ) -> Self {
        let runtime =
            SharedAnalyzerRuntime::new_registered(Arc::clone(&world), &view, registry, key);

        Self {
            world,
            view,
            runtime,
        }
    }

    pub fn workspaces(&self) -> anyhow::Result<Vec<ProjectWorkspace>> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;

        Ok(world.workspaces(&self.view))
    }

    pub fn runtime(&self) -> SharedAnalyzerRuntime {
        self.runtime.clone()
    }
}

#[derive(Clone)]
pub struct SharedAnalyzerRuntime {
    world: Arc<Mutex<SharedWorld>>,
    session: Arc<SharedAnalyzerRuntimeSession>,
}

struct SharedAnalyzerRuntimeSession {
    world: Arc<Mutex<SharedWorld>>,
    id: u64,
    activity: Arc<AtomicBool>,
    input_generation: Arc<AtomicU64>,
    config_generation_seen: AtomicU64,
    edit_generation: AtomicU64,
    workspace_indexes: Vec<usize>,
    excluded_paths: Vec<String>,
    line_endings: Mutex<SharedLineEndings>,
    file_mappings: Mutex<SharedFileMappings>,
    registry_lease: Option<SharedAnalyzerRegistryLease>,
}

impl Drop for SharedAnalyzerRuntimeSession {
    fn drop(&mut self) {
        if let Ok(mut world) = self.world.lock() {
            world.unregister_session(self.id);
        }
        if let Some(lease) = &self.registry_lease
            && let Some(registry) = lease.registry.upgrade()
        {
            registry.unregister(&lease.key);
        }
    }
}

impl std::fmt::Debug for SharedAnalyzerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedAnalyzerRuntime")
            .field("session", &self.session_id())
            .finish_non_exhaustive()
    }
}

impl SharedAnalyzerRuntime {
    fn new(world: Arc<Mutex<SharedWorld>>, view: &WorkspaceView) -> Self {
        Self::new_with_registry(
            world,
            view.workspace_indexes().collect(),
            view.excluded_paths().to_vec(),
            None,
        )
    }

    fn new_registered(
        world: Arc<Mutex<SharedWorld>>,
        view: &WorkspaceView,
        registry: Weak<SharedAnalyzerRegistry>,
        key: SharedAnalyzerBackendKey,
    ) -> Self {
        Self::new_with_registry(
            world,
            view.workspace_indexes().collect(),
            view.excluded_paths().to_vec(),
            Some(SharedAnalyzerRegistryLease { registry, key }),
        )
    }

    fn new_with_registry(
        world: Arc<Mutex<SharedWorld>>,
        workspace_indexes: Vec<usize>,
        excluded_paths: Vec<String>,
        registry_lease: Option<SharedAnalyzerRegistryLease>,
    ) -> Self {
        let (id, activity, input_generation) = world
            .lock()
            .expect("shared world mutex poisoned")
            .register_session();
        let session = Arc::new(SharedAnalyzerRuntimeSession {
            world: Arc::clone(&world),
            id,
            activity,
            input_generation,
            config_generation_seen: AtomicU64::new(u64::MAX),
            edit_generation: AtomicU64::new(0),
            workspace_indexes,
            excluded_paths,
            line_endings: Mutex::new(SharedLineEndings::default()),
            file_mappings: Mutex::new(SharedFileMappings::default()),
            registry_lease,
        });

        let runtime = Self { world, session };
        runtime.refresh_session_cache_from_world();
        runtime
    }

    fn session_id(&self) -> u64 {
        self.session.id
    }

    pub(crate) fn set_busy(&self, busy: bool) {
        self.session.activity.store(busy, Ordering::SeqCst);
    }

    pub(crate) fn config_generation_changed(&self) -> bool {
        let generation = self.session.input_generation.load(Ordering::SeqCst);
        self.session
            .config_generation_seen
            .swap(generation, Ordering::SeqCst)
            != generation
    }

    pub(crate) fn edit_generation(&self) -> u64 {
        self.session.edit_generation.load(Ordering::SeqCst)
    }

    fn workspace_indexes(&self) -> &[usize] {
        &self.session.workspace_indexes
    }

    fn refresh_session_cache_from_world(&self) {
        let world = self
            .world
            .lock()
            .expect("shared world mutex poisoned");
        self.refresh_session_cache(&world);
    }

    fn refresh_session_cache(&self, world: &SharedWorld) {
        let line_endings =
            world.line_endings_for_session(self.session_id(), self.workspace_indexes());
        let file_mappings =
            world.file_mappings_for_session(self.session_id(), self.workspace_indexes());
        *self
            .session
            .line_endings
            .lock()
            .expect("shared analyzer line endings mutex poisoned") = line_endings;
        *self
            .session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned") = file_mappings;
    }

    pub(crate) fn analysis(&self) -> Analysis {
        let world = self
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let visible_files = world.visible_crate_roots_for_session(
            self.session_id(),
            self.workspace_indexes(),
            &self.session.excluded_paths,
        );
        self.refresh_session_cache(&world);
        world
            .host
            .analyzed_analysis_with_visible_files(Arc::new(visible_files))
    }

    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        let path = crate::lsp::from_proto::vfs_path(url)?;
        self.vfs_path_to_file_id(&path)
    }

    pub(crate) fn base_url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        let path = crate::lsp::from_proto::vfs_path(url)?;
        self.base_vfs_path_to_file_id(&path)
    }

    pub(crate) fn vfs_path_to_file_id(&self, path: &VfsPath) -> anyhow::Result<Option<FileId>> {
        let path = normalize_vfs_path(path);
        Ok(self
            .session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .file_id(&path))
    }

    pub(crate) fn base_vfs_path_to_file_id(
        &self,
        path: &VfsPath,
    ) -> anyhow::Result<Option<FileId>> {
        let path = normalize_vfs_path(path);
        Ok(self
            .session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .base_file_id(&path))
    }

    pub(crate) fn file_id_to_url(&self, file_id: FileId) -> Option<Url> {
        let path = self.file_id_to_vfs_path(file_id)?;
        let path = path.as_path()?;
        Some(crate::lsp::to_proto::url_from_abs_path(path))
    }

    pub(crate) fn file_id_to_vfs_path(&self, file_id: FileId) -> Option<VfsPath> {
        self.session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .path(file_id)
    }

    pub(crate) fn line_endings(&self, file_id: FileId) -> Option<crate::line_index::LineEndings> {
        self.session
            .line_endings
            .lock()
            .expect("shared analyzer line endings mutex poisoned")
            .get(file_id)
    }

    pub(crate) fn file_exists(&self, file_id: FileId) -> Option<bool> {
        self.session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .exists(file_id)
    }

    pub(crate) fn ratoml_files(
        &self,
    ) -> Vec<(VfsPath, SourceRootId, bool, String)> {
        let world = self
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let db = world.host.raw_database();
        let mut files = Vec::new();

        for workspace in world.loaded_workspaces_in(self.workspace_indexes()) {
            for (file_id, path) in workspace._vfs.iter() {
                if !workspace._vfs.exists(file_id)
                    || path.name_and_extension() != Some(("rust-analyzer", Some("toml")))
                {
                    continue;
                }
                let source_root_id = db.file_source_root(file_id).source_root_id(db);
                let source_root = db.source_root(source_root_id).source_root(db);
                let text = db.file_text(file_id).text(db).to_string();
                files.push((path.clone(), source_root_id, source_root.is_library, text));
            }
        }

        if let Some(overlay) = world.session_overlays.get(&self.session_id()) {
            for file in overlay.files_by_path.values() {
                if file.display_path.name_and_extension() == Some(("rust-analyzer", Some("toml"))) {
                    files.push((
                        file.display_path.clone(),
                        file.base_source_root,
                        false,
                        file.text.clone(),
                    ));
                }
            }
        }

        files
    }

    pub(crate) fn source_root_parent_map(&self) -> FxHashMap<SourceRootId, SourceRootId> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .source_root_parent_map(self.workspace_indexes())
    }

    pub(crate) fn source_root_for_path(&self, path: &VfsPath) -> Option<(SourceRootId, bool)> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .source_root_for_path(self.workspace_indexes(), path)
    }

    pub(crate) fn sync_open_files(
        &self,
        files: Vec<(
            VfsPath,
            VfsPath,
            String,
            crate::line_index::LineEndings,
        )>,
    ) -> anyhow::Result<SharedOverlaySync> {
        let mut world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let sync = world.sync_session_overlay(self.session_id(), self.workspace_indexes(), files)?;
        if sync.changed {
            self.session.edit_generation.fetch_add(1, Ordering::SeqCst);
        }
        self.refresh_session_cache(&world);
        Ok(sync)
    }

    pub(crate) fn overlay_needed(
        &self,
        files: &[(VfsPath, String, crate::line_index::LineEndings)],
    ) -> anyhow::Result<bool> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        if world
            .session_overlays
            .get(&self.session_id())
            .is_some_and(|overlay| !overlay.files_by_path.is_empty())
        {
            return Ok(true);
        }

        let db = world.host.raw_database();
        for (path, text, _) in files {
            let Some(base_file) = world
                .base_file_for_vfs_path_in(self.workspace_indexes(), &normalize_vfs_path(path))
            else {
                continue;
            };
            if db.file_text(base_file).text(db).as_ref() != text.as_str() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub(crate) fn prepare_overlay_files(
        &self,
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Vec<(VfsPath, VfsPath, String, crate::line_index::LineEndings)>> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        world.prepare_session_overlay_files(self.workspace_indexes(), files)
    }
}

fn normalize_vfs_path(path: &VfsPath) -> VfsPath {
    let Some(path) = path.as_path() else {
        return path.clone();
    };
    let Ok(path) = std::fs::canonicalize(path) else {
        return VfsPath::from(path.to_path_buf());
    };

    VfsPath::from(AbsPathBuf::assert_utf8(path))
}

fn path_key(path: &VfsPath) -> String {
    normalize_vfs_path(path).to_string()
}

fn allocate_shared_file_id() -> FileId {
    const MAX_ANALYZED_FILE_ID: u32 = 0x007F_FFFF;
    static NEXT_FILE_ID: AtomicU32 = AtomicU32::new(0);

    let file_id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    assert!(
        file_id <= MAX_ANALYZED_FILE_ID,
        "shared analyzer file id overflowed"
    );

    FileId::from_raw(file_id)
}

#[derive(Clone, Default)]
struct SharedLineEndings {
    workspaces: Vec<Arc<BTreeMap<FileId, crate::line_index::LineEndings>>>,
    overlay: BTreeMap<FileId, crate::line_index::LineEndings>,
}

impl SharedLineEndings {
    fn get(&self, file_id: FileId) -> Option<crate::line_index::LineEndings> {
        for line_endings in &self.workspaces {
            if let Some(line_endings) = line_endings.get(&file_id) {
                return Some(*line_endings);
            }
        }

        self.overlay.get(&file_id).copied()
    }
}

#[derive(Clone, Default)]
struct SharedFileMappings {
    workspaces: Vec<Arc<LoadedWorkspaceFiles>>,
    overlay_by_path: BTreeMap<String, FileId>,
    overlay_by_file: BTreeMap<FileId, VfsPath>,
}

impl SharedFileMappings {
    fn file_id(&self, path: &VfsPath) -> Option<FileId> {
        let key = path_key(path);
        if let Some(file_id) = self.overlay_by_path.get(&key).copied() {
            return Some(file_id);
        }

        self.base_file_id(path)
    }

    fn base_file_id(&self, path: &VfsPath) -> Option<FileId> {
        self.workspaces
            .iter()
            .find_map(|workspace| workspace.file_id(path).map(|(file_id, _)| file_id))
    }

    fn path(&self, file_id: FileId) -> Option<VfsPath> {
        if let Some(path) = self.overlay_by_file.get(&file_id) {
            return Some(path.clone());
        }

        self.workspaces
            .iter()
            .find_map(|workspace| workspace.path(file_id).cloned())
    }

    fn exists(&self, file_id: FileId) -> Option<bool> {
        if self.overlay_by_file.contains_key(&file_id) {
            return Some(true);
        }

        self.workspaces.iter().find_map(|workspace| {
            workspace
                .contains_file(file_id)
                .then(|| workspace.exists(file_id))
        })
    }
}

fn common_path_prefix_len(left: &str, right: &str) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut index = 0;
    let mut last_separator = 0;
    let end = left.len().min(right.len());

    while index < end && left[index] == right[index] {
        if left[index] == b'/' {
            last_separator = index + 1;
        }
        index += 1;
    }

    if index == end
        && (left.len() == right.len()
            || left.get(index) == Some(&b'/')
            || right.get(index) == Some(&b'/'))
    {
        return index;
    }

    last_separator
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PackageInstanceKey {
    pub root_file: String,
    pub edition: String,
    pub origin: String,
    pub display_name: String,
    pub version: String,
    pub cfg_options: String,
    pub env: String,
    pub is_proc_macro: bool,
    pub proc_macro_cwd: String,
}

#[derive(Clone, Debug)]
pub struct PackageInstance {
    key: PackageInstanceKey,
    crates: Vec<ide::Crate>,
}

impl PackageInstance {
    fn new(key: PackageInstanceKey) -> Self {
        Self {
            key,
            crates: Vec::new(),
        }
    }

    fn push_crate(&mut self, krate: ide::Crate) {
        if !self.crates.contains(&krate) {
            self.crates.push(krate);
        }
    }

    pub fn key(&self) -> &PackageInstanceKey {
        &self.key
    }

    pub fn crates(&self) -> &[ide::Crate] {
        &self.crates
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionOverlay {
    files: Vec<SessionOverlayFile>,
    crates: Vec<SessionOverlayCrate>,
}

impl SessionOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn files(&self) -> &[SessionOverlayFile] {
        &self.files
    }

    pub fn crates(&self) -> &[SessionOverlayCrate] {
        &self.crates
    }

    pub fn push_file(&mut self, file: SessionOverlayFile) {
        if !self.files.iter().any(|it| it.base_file == file.base_file) {
            self.files.push(file);
        }
    }

    pub fn push_crate(&mut self, krate: SessionOverlayCrate) {
        if !self.crates.iter().any(|it| it.base_crate == krate.base_crate) {
            self.crates.push(krate);
        }
    }

    pub fn materialize_files(&mut self) {
        for file in &mut self.files {
            if file.session_file.is_none() {
                file.session_file = Some(allocate_shared_file_id());
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionOverlayFile {
    base_file: FileId,
    session_file: Option<FileId>,
    path: VfsPath,
}

impl SessionOverlayFile {
    pub fn new(base_file: FileId, session_file: FileId, path: VfsPath) -> Self {
        Self {
            base_file,
            session_file: Some(session_file),
            path,
        }
    }

    pub fn pending(base_file: FileId, path: VfsPath) -> Self {
        Self {
            base_file,
            session_file: None,
            path,
        }
    }

    pub fn base_file(&self) -> FileId {
        self.base_file
    }

    pub fn session_file(&self) -> Option<FileId> {
        self.session_file
    }

    pub fn path(&self) -> &VfsPath {
        &self.path
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionOverlayCrate {
    base_crate: ide::Crate,
    session_crate: Option<ide::Crate>,
}

impl SessionOverlayCrate {
    pub fn new(base_crate: ide::Crate, session_crate: ide::Crate) -> Self {
        Self {
            base_crate,
            session_crate: Some(session_crate),
        }
    }

    pub fn pending(base_crate: ide::Crate) -> Self {
        Self {
            base_crate,
            session_crate: None,
        }
    }

    pub fn shared(krate: ide::Crate) -> Self {
        Self::new(krate, krate)
    }

    pub fn base_crate(&self) -> ide::Crate {
        self.base_crate
    }

    pub fn session_crate(&self) -> Option<ide::Crate> {
        self.session_crate
    }

    pub fn is_shared(&self) -> bool {
        self.session_crate == Some(self.base_crate)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SharedOverlaySync {
    pub(crate) changed: bool,
    pub(crate) removed_files: Vec<FileId>,
}

#[derive(Clone, Debug, Default)]
struct ActiveSessionOverlay {
    workspaces: Vec<usize>,
    open_files: BTreeMap<String, OpenOverlayFile>,
    files_by_path: BTreeMap<String, ActiveOverlayFile>,
    path_by_file: BTreeMap<FileId, String>,
    crates: BTreeMap<ide::Crate, FileId>,
}

impl ActiveSessionOverlay {
    fn file_ids(&self) -> impl Iterator<Item = FileId> + '_ {
        self.path_by_file.keys().copied()
    }

    fn file_mappings(&self) -> (BTreeMap<String, FileId>, BTreeMap<FileId, VfsPath>) {
        let by_path = self
            .files_by_path
            .iter()
            .map(|(key, file)| (key.clone(), file.overlay_file))
            .collect();
        let by_file = self
            .files_by_path
            .values()
            .map(|file| (file.overlay_file, file.display_path.clone()))
            .collect();

        (by_path, by_file)
    }
}

#[derive(Clone, Debug)]
struct OpenOverlayFile {
    overlay_file: FileId,
    text: String,
}

#[derive(Clone, Debug)]
struct ActiveOverlayFile {
    overlay_file: FileId,
    base_source_root: SourceRootId,
    path: VfsPath,
    display_path: VfsPath,
    text: String,
    line_endings: crate::line_index::LineEndings,
}

struct LoadedWorkspaceInput {
    source_roots: Vec<SourceRoot>,
    crate_graph: CrateGraphBuilder,
    proc_macros: Vec<AnalyzedProcMacroLoad>,
}

struct LoadedWorkspaceFile {
    path: VfsPath,
    exists: bool,
}

struct LoadedWorkspaceFiles {
    files_by_id: BTreeMap<FileId, LoadedWorkspaceFile>,
    file_ids_by_path: BTreeMap<String, FileId>,
}

impl LoadedWorkspaceFiles {
    fn from_vfs(vfs: &Vfs, file_id_map: &FxHashMap<FileId, FileId>) -> Self {
        let mut files_by_id = BTreeMap::new();
        let mut file_ids_by_path = BTreeMap::new();

        for (old_file_id, path) in vfs.iter() {
            let Some(&file_id) = file_id_map.get(&old_file_id) else {
                continue;
            };
            files_by_id.insert(
                file_id,
                LoadedWorkspaceFile {
                    path: path.clone(),
                    exists: vfs.exists(old_file_id),
                },
            );
            file_ids_by_path.insert(path_key(path), file_id);
        }

        Self {
            files_by_id,
            file_ids_by_path,
        }
    }

    fn iter(&self) -> impl Iterator<Item = (FileId, &VfsPath)> {
        self.files_by_id
            .iter()
            .map(|(&file_id, file)| (file_id, &file.path))
    }

    fn file_id(&self, path: &VfsPath) -> Option<(FileId, ())> {
        self.file_ids_by_path
            .get(&path_key(path))
            .copied()
            .map(|file_id| (file_id, ()))
    }

    fn exists(&self, file_id: FileId) -> bool {
        self.files_by_id
            .get(&file_id)
            .is_some_and(|file| file.exists)
    }

    fn contains_file(&self, file_id: FileId) -> bool {
        self.files_by_id.contains_key(&file_id)
    }

    fn path(&self, file_id: FileId) -> Option<&VfsPath> {
        self.files_by_id
            .get(&file_id)
            .map(|file| &file.path)
    }
}

struct LoadedWorkspace {
    summary: WorkspaceSummary,
    workspace: ProjectWorkspace,
    input: LoadedWorkspaceInput,
    _vfs: Arc<LoadedWorkspaceFiles>,
    line_endings: Arc<BTreeMap<FileId, crate::line_index::LineEndings>>,
    source_root_parent_map: FxHashMap<SourceRootId, SourceRootId>,
    _proc_macro_client: Option<ProcMacroClient>,
}

impl LoadedWorkspace {
    fn summary(&self) -> &WorkspaceSummary {
        &self.summary
    }
}

struct PreparedWorkspaceLoad {
    root_key: String,
    summary: WorkspaceSummary,
    workspace: ProjectWorkspace,
    loaded: AnalyzedWorkspaceLoad,
    line_endings: BTreeMap<FileId, crate::line_index::LineEndings>,
}

pub struct SharedWorld {
    host: AnalysisHost,
    loaded_workspaces: Vec<LoadedWorkspace>,
    workspace_indexes: BTreeMap<String, usize>,
    package_instances: BTreeMap<PackageInstanceKey, PackageInstance>,
    base_crates: Vec<ide::Crate>,
    base_max_source_root: Option<u32>,
    session_overlays: BTreeMap<u64, ActiveSessionOverlay>,
    session_activity: BTreeMap<u64, Arc<AtomicBool>>,
    input_generation: Arc<AtomicU64>,
    applied_source_roots: Vec<SourceRoot>,
    applied_local_roots: rustc_hash::FxHashSet<SourceRootId>,
    applied_library_roots: rustc_hash::FxHashSet<SourceRootId>,
    applied_overlay_files: rustc_hash::FxHashSet<FileId>,
    next_session_id: u64,
}

impl SharedWorld {
    pub fn new() -> Self {
        Self {
            host: AnalysisHost::with_database(RootDatabase::new(None)),
            loaded_workspaces: Vec::new(),
            workspace_indexes: BTreeMap::new(),
            package_instances: BTreeMap::new(),
            base_crates: Vec::new(),
            base_max_source_root: None,
            session_overlays: BTreeMap::new(),
            session_activity: BTreeMap::new(),
            input_generation: Arc::new(AtomicU64::new(0)),
            applied_source_roots: Vec::new(),
            applied_local_roots: rustc_hash::FxHashSet::default(),
            applied_library_roots: rustc_hash::FxHashSet::default(),
            applied_overlay_files: rustc_hash::FxHashSet::default(),
            next_session_id: 1,
        }
    }

    fn workspace_index(&self, load_key: &str) -> Option<usize> {
        self.workspace_indexes.get(load_key).copied()
    }

    fn prepare_workspace_load(
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<PreparedWorkspaceLoad> {
        match source {
            SharedAnalyzerWorkspaceLoadSource::Project(project) => {
                let load_key = shared_project_key(&project);
                let workspace = match project {
                    crate::config::LinkedProject::ProjectManifest(manifest) => {
                        ProjectWorkspace::load(manifest, &config.cargo_config, &|_| {})?
                    }
                    crate::config::LinkedProject::InlineProjectJson(project) => {
                        ProjectWorkspace::load_inline(project, &config.cargo_config, &|_| {})
                    }
                };
                Self::prepare_loaded_workspace(load_key, workspace, config)
            }
            SharedAnalyzerWorkspaceLoadSource::DetachedFile(file) => {
                let load_key = shared_detached_file_key(&file);
                let workspace = ProjectWorkspace::load_detached_files(
                    vec![file],
                    &config.cargo_config,
                )
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::format_err!("detached file did not produce a workspace"))??;
                Self::prepare_loaded_workspace(load_key, workspace, config)
            }
        }
    }

    fn prepare_loaded_workspace(
        load_key: String,
        mut workspace: ProjectWorkspace,
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<PreparedWorkspaceLoad> {
        let manifest_path = workspace
            .manifest()
            .map(ToString::to_string)
            .unwrap_or_else(|| workspace.workspace_root().to_string());
        let summary_root = workspace.workspace_root().to_string();
        let packages = workspace.n_packages();
        if config.load.key.load_out_dirs_from_check {
            let build_scripts = workspace.run_build_scripts(&config.cargo_config, &|_| {})?;
            workspace.set_build_scripts(build_scripts);
        }
        let workspace_for_session = workspace.clone();
        let loaded = analyzed_load_workspace_change(
            workspace,
            &config.cargo_config.extra_env,
            &config.load.to_load_cargo_config(),
            |_| allocate_shared_file_id(),
        )?;
        let files = loaded.vfs.iter().count();
        let line_endings = loaded
            .file_texts
            .iter()
            .map(|(file_id, text)| {
                let (_, line_endings) =
                    crate::line_index::LineEndings::normalize(text.clone());
                (*file_id, line_endings)
            })
            .collect();
        let proc_macro_server = loaded.proc_macro_server.is_some();

        Ok(PreparedWorkspaceLoad {
            root_key: load_key,
            summary: WorkspaceSummary {
                root: summary_root,
                manifest: manifest_path,
                packages,
                files,
                proc_macro_server,
            },
            workspace: workspace_for_session,
            loaded,
            line_endings,
        })
    }

    fn commit_workspace(
        &mut self,
        loaded: PreparedWorkspaceLoad,
        reload: bool,
    ) -> anyhow::Result<usize> {
        let result = self.commit_workspace_inner(loaded, reload);
        if result.is_ok() {
            self.input_generation.fetch_add(1, Ordering::SeqCst);
        }
        result
    }

    fn commit_workspace_inner(
        &mut self,
        loaded: PreparedWorkspaceLoad,
        reload: bool,
    ) -> anyhow::Result<usize> {
        if let Some(&index) = self.workspace_indexes.get(&loaded.root_key) {
            if !reload {
                return Ok(index);
            }

            let (workspace, file_texts) = self.remap_workspace_load(loaded);
            let removed_files = self.loaded_workspaces[index]
                ._vfs
                .iter()
                .map(|(file_id, _)| file_id)
                .collect::<Vec<_>>();
            self.loaded_workspaces[index] = workspace;
            let (source_roots, mut change) = self.base_input_change(file_texts);
            for file_id in removed_files {
                change.change_file(file_id, None);
            }

            self.host.raw_database_mut().enable_proc_attr_macros();
            self.apply_source_roots(source_roots);
            self.host.apply_change(change);
            self.refresh_base_inputs();
            let removed_overlay_files = self.recone_session_overlays()?;
            self.rebuild_overlay_inputs(removed_overlay_files)?;
            self.refresh_package_instances()?;
            return Ok(index);
        }

        let root_key = loaded.root_key.clone();
        let (workspace, file_texts) = self.remap_workspace_load(loaded);
        let index = self.loaded_workspaces.len();
        self.workspace_indexes.insert(root_key, index);
        self.loaded_workspaces.push(workspace);
        let (source_roots, change) = self.base_input_change(file_texts);

        self.host.raw_database_mut().enable_proc_attr_macros();
        self.apply_source_roots(source_roots);
        self.host.apply_change(change);
        self.refresh_base_inputs();
        self.refresh_package_instances()?;
        Ok(index)
    }

    fn remap_workspace_load(
        &self,
        loaded: PreparedWorkspaceLoad,
    ) -> (LoadedWorkspace, Vec<(FileId, String)>) {
        let files = LoadedWorkspaceFiles::from_vfs(&loaded.loaded.vfs, &loaded.loaded.file_id_map);
        let file_texts = loaded.loaded.file_texts.clone();
        let line_endings = Arc::new(loaded.line_endings.into_iter().collect());
        let source_root_parent_map = loaded.loaded.source_root_parent_map.into_iter().collect();

        (
            LoadedWorkspace {
                summary: loaded.summary,
                workspace: loaded.workspace,
                input: LoadedWorkspaceInput {
                    source_roots: loaded.loaded.source_roots,
                    crate_graph: loaded.loaded.crate_graph,
                    proc_macros: loaded.loaded.proc_macros,
                },
                _vfs: Arc::new(files),
                line_endings,
                source_root_parent_map,
                _proc_macro_client: loaded.loaded.proc_macro_server,
            },
            file_texts,
        )
    }

    fn base_input_change(
        &self,
        file_texts: Vec<(FileId, String)>,
    ) -> (Vec<SourceRoot>, ChangeWithProcMacros) {
        let mut change = ChangeWithProcMacros::default();
        let mut source_roots = Vec::new();
        let mut crate_graph = CrateGraphBuilder::default();
        let mut proc_macros = ProcMacrosBuilder::default();

        for input in self
            .loaded_workspaces
            .iter()
            .map(|workspace| &workspace.input)
        {
            source_roots.extend(input.source_roots.iter().cloned());
            let mut proc_macro_paths = ProcMacroPaths::default();
            let crate_id_map = crate_graph.extend(input.crate_graph.clone(), &mut proc_macro_paths);
            for (crate_id, proc_macro) in &input.proc_macros {
                if let Some(crate_id) = crate_id_map.get(crate_id).copied() {
                    proc_macros.insert(crate_id, proc_macro.clone());
                }
            }
        }

        change.set_crate_graph(crate_graph);
        change.set_proc_macros(proc_macros);
        for (file_id, text) in file_texts {
            change.change_file(file_id, Some(text));
        }

        (source_roots, change)
    }

    fn apply_source_roots(&mut self, roots: Vec<SourceRoot>) {
        let db = self.host.raw_database_mut();
        let mut local_roots = rustc_hash::FxHashSet::default();
        let mut library_roots = rustc_hash::FxHashSet::default();
        for (index, root) in roots.iter().enumerate() {
            let root_id = SourceRootId(index as u32);
            if root.is_library {
                library_roots.insert(root_id);
            } else {
                local_roots.insert(root_id);
            }
        }

        for (index, root) in roots.into_iter().enumerate() {
            let root_id = SourceRootId(index as u32);
            let previous = self.applied_source_roots.get(index);
            if previous == Some(&root) {
                continue;
            }

            let durability = if root.is_library {
                Durability::MEDIUM
            } else {
                Durability::LOW
            };
            for file_id in root.iter() {
                if previous.is_none_or(|previous| previous.path_for_file(&file_id).is_none()) {
                    db.set_file_source_root_with_durability(file_id, root_id, durability);
                }
            }
            db.set_source_root_with_durability(
                root_id,
                triomphe::Arc::new(root.clone()),
                durability,
            );
            if index < self.applied_source_roots.len() {
                self.applied_source_roots[index] = root;
            } else {
                self.applied_source_roots.push(root);
            }
        }

        if self.applied_local_roots != local_roots {
            LocalRoots::get(db).set_roots(db).to(local_roots.clone());
            self.applied_local_roots = local_roots;
        }
        if self.applied_library_roots != library_roots {
            LibraryRoots::get(db).set_roots(db).to(library_roots.clone());
            self.applied_library_roots = library_roots;
        }
    }

    fn register_session(&mut self) -> (u64, Arc<AtomicBool>, Arc<AtomicU64>) {
        let id = self.next_session_id;
        self.next_session_id += 1;
        self.session_overlays
            .insert(id, ActiveSessionOverlay::default());
        let activity = Arc::new(AtomicBool::new(true));
        self.session_activity.insert(id, Arc::clone(&activity));
        (id, activity, Arc::clone(&self.input_generation))
    }

    fn unregister_session(&mut self, session_id: u64) {
        self.session_activity.remove(&session_id);
        let old_files = self
            .session_overlays
            .remove(&session_id)
            .into_iter()
            .flat_map(|overlay| overlay.file_ids().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if !old_files.is_empty()
            && let Err(error) = self.rebuild_overlay_inputs(old_files)
        {
            tracing::error!("failed to unregister shared analyzer session {session_id}: {error}");
        }
    }

    pub fn workspace_summary(&self, index: usize) -> Option<&WorkspaceSummary> {
        self.loaded_workspaces.get(index).map(LoadedWorkspace::summary)
    }

    pub fn workspaces(&self, view: &WorkspaceView) -> Vec<ProjectWorkspace> {
        view.workspace_indexes()
            .filter_map(|index| self.loaded_workspaces.get(index))
            .map(|workspace| workspace.workspace.clone())
            .collect()
    }

    fn workspace_summaries(&self, view: &WorkspaceView) -> Vec<WorkspaceSummary> {
        view.workspace_indexes()
            .filter_map(|index| self.loaded_workspaces.get(index))
            .map(|workspace| workspace.summary().clone())
            .collect()
    }

    fn loaded_workspaces_in<'a>(
        &'a self,
        workspaces: &'a [usize],
    ) -> impl Iterator<Item = &'a LoadedWorkspace> + 'a {
        workspaces
            .iter()
            .filter_map(|&index| self.loaded_workspaces.get(index))
    }

    fn synthetic_write(&mut self) {
        self.host
            .raw_database_mut()
            .synthetic_write(Durability::LOW);
    }

    fn any_session_busy(&self) -> bool {
        self.session_activity
            .values()
            .any(|activity| activity.load(Ordering::SeqCst))
    }

    pub fn workspace_file(&self, path: impl AsRef<Path>) -> anyhow::Result<(FileId, VfsPath)> {
        self.workspace_file_in(0..self.loaded_workspaces.len(), path)
    }

    fn workspace_file_in(
        &self,
        workspaces: impl IntoIterator<Item = usize>,
        path: impl AsRef<Path>,
    ) -> anyhow::Result<(FileId, VfsPath)> {
        let path = VfsPath::from(AbsPathBuf::assert_utf8(std::fs::canonicalize(path)?));

        for workspace in workspaces {
            let Some(workspace) = self.loaded_workspaces.get(workspace) else {
                continue;
            };
            for (file_id, vfs_path) in workspace._vfs.iter() {
                if *vfs_path == path {
                    return Ok((file_id, vfs_path.clone()));
                }
            }
        }

        anyhow::bail!("workspace file is not loaded: {path}")
    }

    fn line_endings_for_session(
        &self,
        session_id: u64,
        workspaces: &[usize],
    ) -> SharedLineEndings {
        let workspaces = self
            .loaded_workspaces_in(workspaces)
            .map(|workspace| Arc::clone(&workspace.line_endings))
            .collect();
        let mut overlay_line_endings = BTreeMap::new();

        if let Some(session_overlay) = self.session_overlays.get(&session_id) {
            overlay_line_endings.extend(session_overlay.files_by_path.values().map(|file| {
                (file.overlay_file, file.line_endings)
            }));
        }

        SharedLineEndings {
            workspaces,
            overlay: overlay_line_endings,
        }
    }

    fn file_mappings_for_session(
        &self,
        session_id: u64,
        workspaces: &[usize],
    ) -> SharedFileMappings {
        let workspaces = self
            .loaded_workspaces_in(workspaces)
            .map(|workspace| Arc::clone(&workspace._vfs))
            .collect();
        let (overlay_by_path, overlay_by_file) = self
            .session_overlays
            .get(&session_id)
            .map(ActiveSessionOverlay::file_mappings)
            .unwrap_or_default();

        SharedFileMappings {
            workspaces,
            overlay_by_path,
            overlay_by_file,
        }
    }

    pub(crate) fn source_root_parent_map(
        &self,
        workspaces: &[usize],
    ) -> FxHashMap<SourceRootId, SourceRootId> {
        let mut map = FxHashMap::default();
        let mut offset = 0;
        for (index, workspace) in self.loaded_workspaces.iter().enumerate() {
            if workspaces.contains(&index) {
                map.extend(workspace.source_root_parent_map.iter().map(
                    |(&source_root, &parent)| {
                        (
                            SourceRootId(source_root.0 + offset),
                            SourceRootId(parent.0 + offset),
                        )
                    },
                ));
            }
            offset += workspace.input.source_roots.len() as u32;
        }

        map
    }

    pub(crate) fn source_root_for_path(
        &self,
        workspaces: &[usize],
        path: &VfsPath,
    ) -> Option<(SourceRootId, bool)> {
        let path = normalize_vfs_path(path);
        if let Some(file_id) = self.base_file_for_vfs_path_in(workspaces, &path) {
            let db = self.host.raw_database();
            let source_root_id = db.file_source_root(file_id).source_root_id(db);
            let source_root = db.source_root(source_root_id).source_root(db);
            return Some((source_root_id, source_root.is_library));
        }

        let path = path_key(&path);
        let mut best = None::<(usize, SourceRootId, bool)>;
        for workspace in self.loaded_workspaces_in(workspaces) {
            for (file_id, file_path) in workspace._vfs.iter() {
                let len = common_path_prefix_len(&path, &path_key(file_path));
                if len == 0 {
                    continue;
                }

                let replace = match best {
                    Some((best_len, _, _)) => len > best_len,
                    None => true,
                };
                if replace {
                    let db = self.host.raw_database();
                    let source_root_id = db.file_source_root(file_id).source_root_id(db);
                    let source_root = db.source_root(source_root_id).source_root(db);
                    best = Some((len, source_root_id, source_root.is_library));
                }
            }
        }

        best.map(|(_, source_root_id, is_library)| (source_root_id, is_library))
    }

    pub(crate) fn sync_session_overlay(
        &mut self,
        session_id: u64,
        workspaces: &[usize],
        files: Vec<(
            VfsPath,
            VfsPath,
            String,
            crate::line_index::LineEndings,
        )>,
    ) -> anyhow::Result<SharedOverlaySync> {
        let open_files = files
            .into_iter()
            .filter_map(|(path, display_path, text, line_endings)| {
                let key = path_key(&path);
                self.base_file_for_vfs_path_in(workspaces, &normalize_vfs_path(&path))
                    .and_then(|base_file| {
                        let db = self.host.raw_database();
                        let base_text = db.file_text(base_file).text(db);
                        if base_text.as_ref() == text.as_str() {
                            return None;
                        }
                        Some(
                        (
                            key,
                            (path, display_path, base_file, text, line_endings),
                        )
                        )
                    })
            })
            .collect::<BTreeMap<_, _>>();

        let old_overlay = self.session_overlays.remove(&session_id).unwrap_or_default();
        let same_open_files = old_overlay.open_files.len() == open_files.len()
            && open_files
                .iter()
                .all(|(key, (_, _, _, text, _))| {
                    old_overlay
                        .open_files
                        .get(key)
                        .is_some_and(|old| old.text == *text)
            });
        if same_open_files {
            self.session_overlays.insert(session_id, old_overlay);
            return Ok(SharedOverlaySync {
                changed: false,
                removed_files: Vec::new(),
            });
        }

        let same_file_set = old_overlay.open_files.len() == open_files.len()
            && open_files
                .keys()
                .all(|key| old_overlay.open_files.contains_key(key));
        if same_file_set {
            let mut overlay = old_overlay;
            let mut change = ChangeWithProcMacros::default();
            for (key, (_, _, _, text, line_endings)) in open_files {
                let open = overlay
                    .open_files
                    .get_mut(&key)
                    .expect("overlay file set was checked");
                if open.text == text {
                    continue;
                }
                open.text = text.clone();
                let file = overlay
                    .files_by_path
                    .get_mut(&key)
                    .expect("overlay file set was checked");
                file.text = text.clone();
                file.line_endings = line_endings;
                change.change_file(open.overlay_file, Some(text));
            }
            self.session_overlays.insert(session_id, overlay);
            self.host.apply_change(change);
            return Ok(SharedOverlaySync {
                changed: true,
                removed_files: Vec::new(),
            });
        }

        let kept_keys = open_files.keys().cloned().collect::<BTreeSet<_>>();
        let removed_file_ids = old_overlay
            .open_files
            .iter()
            .filter_map(|(key, file)| (!kept_keys.contains(key)).then_some(file.overlay_file))
            .collect::<Vec<_>>();
        let mut overlay = ActiveSessionOverlay {
            workspaces: workspaces.to_vec(),
            ..ActiveSessionOverlay::default()
        };

        for (key, (path, display_path, base_file, text, line_endings)) in open_files {
            let overlay_file = old_overlay
                .open_files
                .get(&key)
                .map(|file| file.overlay_file)
            .unwrap_or_else(|| self.allocate_overlay_file_id());
            if old_overlay
                .open_files
                .get(&key)
                .is_some_and(|old| old.text != text)
            {
                self.applied_overlay_files.remove(&overlay_file);
            }
            overlay.open_files.insert(
                key.clone(),
                OpenOverlayFile {
                    overlay_file,
                    text: text.clone(),
                },
            );
            overlay.path_by_file.insert(overlay_file, key.clone());
            overlay.files_by_path.insert(
                key,
                ActiveOverlayFile {
                    overlay_file,
                    base_source_root: self.source_root_for_file(base_file)?,
                    path,
                    display_path,
                    text,
                    line_endings,
                },
            );
            self.populate_overlay_crates(&mut overlay, base_file)?;
        }

        self.session_overlays.insert(session_id, overlay);
        self.rebuild_overlay_inputs(removed_file_ids.clone())?;

        Ok(SharedOverlaySync {
            changed: true,
            removed_files: removed_file_ids,
        })
    }

    pub(crate) fn prepare_session_overlay_files(
        &self,
        workspaces: &[usize],
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Vec<(VfsPath, VfsPath, String, crate::line_index::LineEndings)>> {
        let open_files = files
            .into_iter()
            .map(|(path, text, line_endings)| {
                let source_path = normalize_vfs_path(&path);
                (path_key(&source_path), (path, text, line_endings))
            })
            .collect::<BTreeMap<_, _>>();
        let mut required_files = BTreeMap::<String, VfsPath>::new();
        let db = self.host.raw_database();

        for (path, _, _) in open_files.values() {
            let source_path = normalize_vfs_path(path);
            let Some(base_file) = self.base_file_for_vfs_path_in(workspaces, &source_path) else {
                continue;
            };

            for krate in self.host.analysis().crates_for(base_file)? {
                let root_file = krate.data(db).root_file_id;
                let source_root_id = self.source_root_for_file(root_file)?;
                let source_root = db.source_root(source_root_id).source_root(db);

                for file_id in source_root.iter() {
                    let Some(path) = source_root.path_for_file(&file_id).cloned() else {
                        continue;
                    };
                    required_files.entry(path_key(&path)).or_insert(path);
                }
            }
        }

        let mut prepared = Vec::new();
        for (key, path) in required_files {
            if let Some((display_path, text, line_endings)) = open_files.get(&key) {
                prepared.push((path, display_path.clone(), text.clone(), *line_endings));
                continue;
            }

            let Some(base_file) = self.base_file_for_vfs_path_in(workspaces, &path) else {
                continue;
            };
            let text = db.file_text(base_file).text(db).to_string();
            let line_endings = self
                .loaded_workspaces_in(workspaces)
                .find_map(|workspace| workspace.line_endings.get(&base_file).copied())
                .unwrap_or_else(|| {
                    let (_, line_endings) =
                        crate::line_index::LineEndings::normalize(text.clone());
                    line_endings
                });
            prepared.push((path.clone(), path, text, line_endings));
        }

        Ok(prepared)
    }

    fn populate_overlay_crates(
        &self,
        overlay: &mut ActiveSessionOverlay,
        base_file: FileId,
    ) -> anyhow::Result<()> {
        let db = self.host.raw_database();

        for krate in self.host.analysis().crates_for(base_file)? {
            let root_file = krate.data(db).root_file_id;
            let source_root_id = self.source_root_for_file(root_file)?;
            let source_root = db.source_root(source_root_id).source_root(db);

            let Some(root_key) = source_root.path_for_file(&root_file).map(path_key) else {
                continue;
            };
            let Some(root_overlay_file) = overlay
                .files_by_path
                .get(&root_key)
                .map(|file| file.overlay_file)
            else {
                continue;
            };
            overlay.crates.insert(krate, root_overlay_file);
        }

        Ok(())
    }

    fn rebuild_overlay_inputs(&mut self, removed_file_ids: Vec<FileId>) -> anyhow::Result<()> {
        let source_roots = self.overlay_source_roots()?;
        let mut change = ChangeWithProcMacros::default();
        change.set_crate_graph(self.overlay_crate_graph()?);

        for file_id in removed_file_ids {
            self.applied_overlay_files.remove(&file_id);
            change.change_file(file_id, None);
        }

        let mut added_files = Vec::new();
        for overlay in self.session_overlays.values() {
            for file in overlay.files_by_path.values() {
                if !self.applied_overlay_files.contains(&file.overlay_file) {
                    added_files.push(file.overlay_file);
                    change.change_file(file.overlay_file, Some(file.text.clone()));
                }
            }
        }
        self.applied_overlay_files.extend(added_files);

        self.apply_source_roots(source_roots);
        self.host.apply_change(change);
        Ok(())
    }

    fn recone_session_overlays(&mut self) -> anyhow::Result<Vec<FileId>> {
        let session_ids = self.session_overlays.keys().copied().collect::<Vec<_>>();
        let mut removed_files = Vec::new();

        for session_id in session_ids {
            let old_overlay = self
                .session_overlays
                .remove(&session_id)
                .unwrap_or_default();
            let mut overlay = ActiveSessionOverlay {
                workspaces: old_overlay.workspaces.clone(),
                ..ActiveSessionOverlay::default()
            };

            for (key, file) in &old_overlay.files_by_path {
                let base_file = self.base_file_for_vfs_path_in(
                    &old_overlay.workspaces,
                    &normalize_vfs_path(&file.path),
                );
                let Some(base_file) = base_file else {
                    removed_files.push(file.overlay_file);
                    continue;
                };
                let base_text = {
                    let db = self.host.raw_database();
                    db.file_text(base_file).text(db)
                };
                if base_text.as_ref() == file.text.as_str() {
                    removed_files.push(file.overlay_file);
                    continue;
                }

                if let Some(open) = old_overlay.open_files.get(key) {
                    overlay.open_files.insert(
                        key.clone(),
                        OpenOverlayFile {
                            overlay_file: open.overlay_file,
                            text: open.text.clone(),
                        },
                    );
                }
                overlay.path_by_file.insert(file.overlay_file, key.clone());
                overlay.files_by_path.insert(
                    key.clone(),
                    ActiveOverlayFile {
                        overlay_file: file.overlay_file,
                        base_source_root: self.source_root_for_file(base_file)?,
                        path: file.path.clone(),
                        display_path: file.display_path.clone(),
                        text: file.text.clone(),
                        line_endings: file.line_endings,
                    },
                );
                self.populate_overlay_crates(&mut overlay, base_file)?;
            }

            self.session_overlays.insert(session_id, overlay);
        }

        Ok(removed_files)
    }

    fn overlay_source_roots(&self) -> anyhow::Result<Vec<SourceRoot>> {
        let db = self.host.raw_database();
        let mut roots = match self.base_max_source_root {
            Some(max_base_root) => (0..=max_base_root)
                .map(|index| {
                    db.source_root(SourceRootId(index))
                        .source_root(db)
                        .as_ref()
                        .clone()
                })
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };

        for overlay in self.session_overlays.values() {
            let mut files_by_root = BTreeMap::<SourceRootId, FileSet>::new();
            for file in overlay.files_by_path.values() {
                files_by_root
                    .entry(file.base_source_root)
                    .or_default()
                    .insert(file.overlay_file, file.path.clone());
            }

            for (base_source_root, file_set) in files_by_root {
                let base = db.source_root(base_source_root).source_root(db);
                let root = if base.is_library {
                    SourceRoot::new_library(file_set)
                } else {
                    SourceRoot::new_local(file_set)
                };
                roots.push(root);
            }
        }

        Ok(roots)
    }

    fn overlay_crate_graph(&self) -> anyhow::Result<CrateGraphBuilder> {
        let db = self.host.raw_database();
        let mut graph = CrateGraphBuilder::default();
        let mut base_builders = BTreeMap::new();
        for krate in &self.base_crates {
            let data = krate.data(db);
            let extra = krate.extra_data(db);
            let builder = graph.add_crate_root(
                data.root_file_id,
                data.edition,
                extra.display_name.clone(),
                extra.version.clone(),
                krate.cfg_options(db).clone(),
                extra.potential_cfg_options.clone(),
                krate.env(db).clone(),
                data.origin.clone(),
                data.crate_attrs.iter().map(|it| it.to_string()).collect(),
                data.is_proc_macro,
                data.proc_macro_cwd.clone(),
                krate.workspace_data(db).clone(),
            );
            base_builders.insert(*krate, builder);
        }

        let mut overlay_builders = BTreeMap::new();
        for (session_id, overlay) in &self.session_overlays {
            for (base_crate, root_file_id) in &overlay.crates {
                let data = base_crate.data(db);
                let extra = base_crate.extra_data(db);
                let builder = graph.add_crate_root(
                    *root_file_id,
                    data.edition,
                    extra.display_name.clone(),
                    extra.version.clone(),
                    base_crate.cfg_options(db).clone(),
                    extra.potential_cfg_options.clone(),
                    base_crate.env(db).clone(),
                    data.origin.clone(),
                    data.crate_attrs.iter().map(|it| it.to_string()).collect(),
                    data.is_proc_macro,
                    data.proc_macro_cwd.clone(),
                    base_crate.workspace_data(db).clone(),
                );
                overlay_builders.insert((*session_id, *base_crate), builder);
            }
        }

        for krate in &self.base_crates {
            let Some(from) = base_builders.get(krate).copied() else {
                continue;
            };
            for dependency in &krate.data(db).dependencies {
                if let Some(to) = base_builders.get(&dependency.crate_id).copied() {
                    graph.add_dep(
                        from,
                        DependencyBuilder::with_prelude(
                            dependency.name.clone(),
                            to,
                            dependency.is_prelude(),
                            dependency.is_sysroot(),
                        ),
                    )
                    .map_err(|error| anyhow::format_err!("{error:?}"))?;
                }
            }
        }

        for ((session_id, base_crate), from) in &overlay_builders {
            for dependency in &base_crate.data(db).dependencies {
                let to = overlay_builders
                    .get(&(*session_id, dependency.crate_id))
                    .or_else(|| base_builders.get(&dependency.crate_id))
                    .copied();
                if let Some(to) = to {
                    graph.add_dep(
                        *from,
                        DependencyBuilder::with_prelude(
                            dependency.name.clone(),
                            to,
                            dependency.is_prelude(),
                            dependency.is_sysroot(),
                        ),
                    )
                    .map_err(|error| anyhow::format_err!("{error:?}"))?;
                }
            }
        }

        graph.shrink_to_fit();
        Ok(graph)
    }

    fn allocate_overlay_file_id(&mut self) -> FileId {
        allocate_shared_file_id()
    }

    fn refresh_base_inputs(&mut self) {
        let db = self.host.raw_database();
        let overlay_files = self
            .session_overlays
            .values()
            .flat_map(ActiveSessionOverlay::file_ids)
            .collect::<BTreeSet<_>>();

        self.base_crates = all_crates(db)
            .iter()
            .copied()
            .filter(|krate| !overlay_files.contains(&krate.data(db).root_file_id))
            .collect();

        self.base_max_source_root = None;
        for workspace in &self.loaded_workspaces {
            for (file_id, _) in workspace._vfs.iter() {
                let source_root = db.file_source_root(file_id).source_root_id(db);
                self.base_max_source_root = Some(
                    self.base_max_source_root
                        .map_or(source_root.0, |max| max.max(source_root.0)),
                );
            }
        }
    }

    fn visible_crate_roots_for_session(
        &self,
        session_id: u64,
        workspaces: &[usize],
        excluded_paths: &[String],
    ) -> rustc_hash::FxHashSet<FileId> {
        let db = self.host.raw_database();
        let overlay = self.session_overlays.get(&session_id);
        let overlay_base_crates = overlay
            .map(|overlay| overlay.crates.keys().copied().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let view_workspaces = self
            .loaded_workspaces_in(workspaces)
            .collect::<Vec<_>>();
        let mut visible_files = rustc_hash::FxHashSet::default();

        for krate in &self.base_crates {
            let root_file = krate.data(db).root_file_id;
            if view_workspaces
                .iter()
                .any(|workspace| workspace._vfs.contains_file(root_file))
                && !overlay_base_crates.contains(krate)
                && !self.file_is_excluded(root_file, excluded_paths)
            {
                visible_files.insert(root_file);
            }
        }

        if let Some(overlay) = overlay {
            visible_files.extend(overlay.crates.values().copied());
        }

        visible_files
    }

    fn file_is_excluded(&self, file_id: FileId, excluded_paths: &[String]) -> bool {
        if excluded_paths.is_empty() {
            return false;
        }

        let Some(path) = self
            .loaded_workspaces
            .iter()
            .find_map(|workspace| workspace._vfs.path(file_id))
        else {
            return false;
        };
        let path = path_key(path);
        excluded_paths
            .iter()
            .any(|excluded| path.starts_with(excluded))
    }

    fn base_file_for_vfs_path_in(&self, workspaces: &[usize], path: &VfsPath) -> Option<FileId> {
        self.loaded_workspaces_in(workspaces)
            .find_map(|workspace| workspace._vfs.file_id(path).map(|(file_id, _)| file_id))
    }

    fn source_root_for_file(&self, file_id: FileId) -> anyhow::Result<SourceRootId> {
        Ok(self
            .host
            .raw_database()
            .file_source_root(file_id)
            .source_root_id(self.host.raw_database()))
    }

    pub fn crate_root_file(&self, krate: ide::Crate) -> anyhow::Result<(FileId, VfsPath)> {
        let db = self.host.raw_database();
        let file_id = krate.data(db).root_file_id;
        let path = path_for_file(db, file_id)?;

        Ok((file_id, VfsPath::new_real_path(path)))
    }

    pub fn package_instances(&self) -> impl Iterator<Item = &PackageInstance> {
        self.package_instances.values()
    }

    pub fn active_overlay_sessions(&self) -> usize {
        self.session_overlays.len()
    }

    pub fn overlay_files(&self) -> usize {
        self.session_overlays
            .values()
            .flat_map(ActiveSessionOverlay::file_ids)
            .count()
    }

    pub fn crates_for_file(&self, file_id: FileId) -> anyhow::Result<Vec<ide::Crate>> {
        Ok(self.host.analysis().crates_for(file_id)?)
    }

    pub fn shared_dependencies(&self, krate: ide::Crate) -> anyhow::Result<Vec<ide::Crate>> {
        let db = self.host.raw_database();
        let mut dependencies = Vec::new();

        for dependency in &krate.data(db).dependencies {
            dependencies.push(self.interned_crate(dependency.crate_id)?);
        }

        Ok(dependencies)
    }

    fn interned_crate(&self, krate: ide::Crate) -> anyhow::Result<ide::Crate> {
        let db = self.host.raw_database();
        let key = package_instance_key(db, krate)?;
        let package = self
            .package_instances
            .get(&key)
            .ok_or_else(|| anyhow::format_err!("package instance is not interned: {:?}", key))?;

        if package.crates().contains(&krate) {
            Ok(krate)
        } else {
            anyhow::bail!("crate is not interned in package instance: {:?}", key)
        }
    }

    fn refresh_package_instances(&mut self) -> anyhow::Result<()> {
        let db = self.host.raw_database();
        self.package_instances.clear();

        for krate in self.base_crates.iter().copied() {
            let key = package_instance_key(db, krate)?;
            match self.package_instances.entry(key.clone()) {
                Entry::Occupied(mut entry) => entry.get_mut().push_crate(krate),
                Entry::Vacant(entry) => {
                    let mut package = PackageInstance::new(key);
                    package.push_crate(krate);
                    entry.insert(package);
                }
            }
        }

        Ok(())
    }
}

impl Default for SharedWorld {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceView {
    workspaces: Vec<usize>,
    excluded_paths: Vec<String>,
}

impl WorkspaceView {
    pub fn new(workspaces: Vec<usize>, excluded_paths: Vec<String>) -> Self {
        Self {
            workspaces,
            excluded_paths,
        }
    }

    pub fn push_workspace(&mut self, workspace: usize) {
        self.workspaces.push(workspace);
    }

    pub fn workspace_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.workspaces.iter().copied()
    }

    pub fn excluded_paths(&self) -> &[String] {
        &self.excluded_paths
    }

    pub fn workspace_summaries<'a>(
        &'a self,
        world: &'a SharedWorld,
    ) -> impl Iterator<Item = &'a WorkspaceSummary> {
        self.workspaces
            .iter()
            .filter_map(|index| world.workspace_summary(*index))
    }

    pub fn workspace_file(
        &self,
        world: &SharedWorld,
        path: impl AsRef<Path>,
    ) -> anyhow::Result<(FileId, VfsPath)> {
        world.workspace_file_in(self.workspaces.iter().copied(), path)
    }

    pub fn overlay_cone(
        &self,
        world: &SharedWorld,
        path: impl AsRef<Path>,
    ) -> anyhow::Result<SessionOverlay> {
        let (base_file, vfs_path) = self.workspace_file(world, path)?;
        let mut overlay = SessionOverlay::new();
        overlay.push_file(SessionOverlayFile::pending(base_file, vfs_path));

        for krate in world.crates_for_file(base_file)? {
            overlay.push_crate(SessionOverlayCrate::pending(krate));
            let (root_file, root_path) = world.crate_root_file(krate)?;
            overlay.push_file(SessionOverlayFile::pending(root_file, root_path));

            for dependency in world.shared_dependencies(krate)? {
                overlay.push_crate(SessionOverlayCrate::shared(dependency));
            }
        }

        overlay.materialize_files();

        Ok(overlay)
    }
}

fn path_for_file(db: &RootDatabase, file_id: FileId) -> anyhow::Result<String> {
    let root = db.file_source_root(file_id).source_root_id(db);
    db.source_root(root)
        .source_root(db)
        .path_for_file(&file_id)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::format_err!("file path is unavailable for {file_id:?}"))
}

fn package_instance_key(db: &RootDatabase, krate: ide::Crate) -> anyhow::Result<PackageInstanceKey> {
    let data = krate.data(db);

    Ok(PackageInstanceKey {
        root_file: path_for_file(db, data.root_file_id)?,
        edition: format!("{:?}", data.edition),
        origin: format!("{:?}", data.origin),
        display_name: format!("{:?}", krate.extra_data(db).display_name),
        version: format!("{:?}", krate.extra_data(db).version),
        cfg_options: format!("{:?}", krate.cfg_options(db)),
        env: format!("{:?}", krate.env(db)),
        is_proc_macro: data.is_proc_macro,
        proc_macro_cwd: format!("{:?}", data.proc_macro_cwd),
    })
}
"#;

const MAIN_LOOP_SESSION_MODULE: &str = r#"

pub(crate) mod analyzed_session {
    use std::{env, fs, sync::Once};

    use crossbeam_channel::{Receiver, Sender};
    use ide::FileId;
    use ide_db::{FxHashMap, base_db::{SourceRoot, SourceRootId}};
    use lsp_server::{Connection, Message};
    use lsp_types::Url;
    use paths::Utf8PathBuf;
    use triomphe::Arc;
    use vfs::{AbsPathBuf, ChangeKind, VfsPath};

    use crate::{
        analyzed_bridge::{SharedAnalyzerProvider, SharedAnalyzerRuntime, patch_path_prefix},
        config::{Config, ConfigChange, ConfigErrors},
        from_json, server_capabilities, version,
        global_state::FetchWorkspaceRequest,
        line_index::LineEndings,
    };

    pub(crate) struct RustAnalyzerSession {
        state: crate::global_state::GlobalState,
    }

    impl RustAnalyzerSession {
        pub(crate) fn new(
            sender: Sender<Message>,
            config: crate::config::Config,
            provider: SharedAnalyzerProvider,
            analyzed_shared: SharedAnalyzerRuntime,
            analyzed_workspaces: Vec<project_model::ProjectWorkspace>,
        ) -> Self {
            Self {
                state: crate::global_state::GlobalState::new(
                    sender,
                    config,
                    provider,
                    analyzed_shared,
                    analyzed_workspaces,
                ),
            }
        }

        pub(crate) fn run_shared(self, receiver: Receiver<Message>) -> anyhow::Result<()> {
            run_shared_state(self.state, receiver)
        }
    }

    pub(crate) fn run_shared_lsp_session(
        connection: Connection,
        provider: SharedAnalyzerProvider,
    ) -> anyhow::Result<()> {
        let (initialize_id, initialize_params) = connection.initialize_start()?;
        tracing::info!("InitializeParams: {}", initialize_params);
        let config = config_from_initialize_params(&connection, &initialize_params)?;
        let initialize_result = lsp_types::InitializeResult {
            capabilities: server_capabilities(&config),
            server_info: Some(lsp_types::ServerInfo {
                name: String::from("rust-analyzer"),
                version: Some(version().to_string()),
            }),
            offset_encoding: None,
        };

        connection.initialize_finish(initialize_id, serde_json::to_value(initialize_result)?)?;

        run_shared_lsp_session_with_config(config, connection, provider)
    }

    pub(crate) fn run_shared_lsp_session_with_config(
        mut config: Config,
        connection: Connection,
        provider: SharedAnalyzerProvider,
    ) -> anyhow::Result<()> {
        if config.discover_workspace_config().is_none()
            && !config.has_linked_projects()
            && config.detached_files().is_empty()
        {
            config.rediscover_workspaces();
        }

        initialize_rayon();
        let (key, shared_config) =
            crate::analyzed_bridge::shared_analyzer_context_from_config(&config)?;
        let session = provider.resolve(key, shared_config)?;
        let analyzed_shared = session.runtime();
        let analyzed_workspaces = Vec::new();
        let Connection { sender, receiver } = connection;
        RustAnalyzerSession::new(sender, config, provider, analyzed_shared, analyzed_workspaces)
            .run_shared(receiver)
    }

    fn run_shared_state(
        mut state: crate::global_state::GlobalState,
        inbox: Receiver<Message>,
    ) -> anyhow::Result<()> {
        if state.config.did_save_text_document_dynamic_registration() {
            let additional_patterns = state
                .config
                .discover_workspace_config()
                .map(|cfg| cfg.files_to_watch.clone().into_iter())
                .into_iter()
                .flatten()
                .map(|file| format!("**/{file}"));
            state.register_did_save_capability(additional_patterns);
        }

        if state.config.discover_workspace_config().is_none() {
            state.fetch_workspaces_queue.request_op(
                "startup".to_owned(),
                FetchWorkspaceRequest { path: None, force_crate_graph_reload: false },
            );
            if let Some((cause, FetchWorkspaceRequest { path, force_crate_graph_reload })) =
                state.fetch_workspaces_queue.should_start_op()
            {
                state.fetch_workspaces(cause, path, force_crate_graph_reload);
            }
        }
        state.update_status_or_notify();

        while let Ok(event) = state.next_event(&inbox) {
            let Some(event) = event else {
                anyhow::bail!("client exited without proper shutdown sequence");
            };
            if matches!(
                &event,
                super::Event::Lsp(lsp_server::Message::Notification(lsp_server::Notification {
                    method,
                    ..
                }))
                if method
                    == <lsp_types::notification::Exit as lsp_types::notification::Notification>::METHOD
            ) {
                return Ok(());
            }
            state.analyzed_shared.set_busy(true);
            state.handle_event(event);
            let idle =
                state.task_pool.handle.is_empty() && state.fmt_pool.handle.is_empty();
            state.analyzed_shared.set_busy(!idle);
        }

        anyhow::bail!("A receiver has been dropped, something panicked!")
    }

    impl crate::global_state::GlobalState {
        pub(crate) fn analyzed_process_shared_changes(&mut self) -> bool {
            let shared = self.analyzed_shared.clone();
            let generation_changed = shared.config_generation_changed();
            let mut modified_ratoml_files = Vec::new();
            let mut workspace_structure_change = None;
            let mut changed = false;

            {
                let mut guard = self.vfs.write();
                let changed_files = guard.0.take_changes();
                if !changed_files.is_empty() {
                    changed = true;
                }

                let additional_files = self
                    .config
                    .discover_workspace_config()
                    .map(|cfg| cfg.files_to_watch.iter().map(String::as_str).collect::<Vec<_>>())
                    .unwrap_or_default();
                let (vfs, line_endings_map) = &mut *guard;

                for file in changed_files.into_values() {
                    let vfs_path = vfs.file_path(file.file_id).clone();
                    let file_kind = file.kind();
                    let file_exists = file.exists();
                    let file_is_created_or_deleted = file.is_created_or_deleted();
                    let text = match file.change {
                        vfs::Change::Create(bytes, _) | vfs::Change::Modify(bytes, _) => {
                            String::from_utf8(bytes).ok().map(|text| {
                                let (text, line_endings) = LineEndings::normalize(text);
                                line_endings_map.insert(file.file_id, line_endings);
                                text
                            })
                        }
                        vfs::Change::Delete => None,
                    };

                    if let Some(("rust-analyzer", Some("toml"))) =
                        vfs_path.name_and_extension()
                    {
                        modified_ratoml_files.push((file_kind, vfs_path.clone(), text.clone()));
                    }

                    if let Some(path) = vfs_path.as_path() {
                        if file_is_created_or_deleted {
                            workspace_structure_change
                                .get_or_insert((path.to_path_buf(), false))
                                .1 |= self.crate_graph_file_dependencies.contains(&vfs_path);
                        } else if crate::reload::should_refresh_for_change(
                            path,
                            file_kind,
                            &additional_files,
                        ) {
                            workspace_structure_change
                                .get_or_insert((path.to_path_buf(), false));
                        }
                    }

                    if !file_exists
                        && let Ok(Some(file_id)) = shared.vfs_path_to_file_id(&vfs_path)
                    {
                        self.diagnostics.clear_native_for(file_id);
                    }
                }
            }

            if changed || generation_changed {
                let open_files = self
                    .mem_docs
                    .iter()
                    .filter_map(|path| {
                        let doc = self.mem_docs.get(path)?;
                        let text = std::str::from_utf8(&doc.data).ok()?.to_owned();
                        let (text, line_endings) = LineEndings::normalize(text);
                        Some((path.clone(), text, line_endings))
                    })
                    .collect::<Vec<_>>();

                let overlay_needed = match shared.overlay_needed(&open_files) {
                    Ok(needed) => needed,
                    Err(error) => {
                        tracing::error!("failed to check shared analyzer overlay: {error}");
                        return false;
                    }
                };
                if overlay_needed {
                    let overlay_files = match shared.prepare_overlay_files(open_files) {
                        Ok(files) => files,
                        Err(error) => {
                            tracing::error!(
                                "failed to prepare shared analyzer overlay: {error}"
                            );
                            return false;
                        }
                    };
                    let sync = match shared.sync_open_files(overlay_files) {
                        Ok(sync) => sync,
                        Err(error) => {
                            tracing::error!("failed to sync shared analyzer overlay: {error}");
                            return false;
                        }
                    };

                    if sync.changed {
                        changed = true;
                    }
                    for file_id in sync.removed_files {
                        self.diagnostics.clear_native_for(file_id);
                    }
                }
            }

            let config_input_changed = generation_changed || !modified_ratoml_files.is_empty();
            if config_input_changed && !self.workspaces.is_empty() {
                let shared_ratoml_files = {
                    let mut files = shared.ratoml_files();
                    files.extend(self.mem_docs.iter().filter_map(|path| {
                        if path.name_and_extension() != Some(("rust-analyzer", Some("toml"))) {
                            return None;
                        }

                        let doc = self.mem_docs.get(path)?;
                        let text = std::str::from_utf8(&doc.data).ok()?.to_owned();
                        let (source_root_id, is_library) = shared.source_root_for_path(path)?;
                        Some((path.clone(), source_root_id, is_library, text))
                    }));
                    files
                };
                let user_config_path = (|| {
                    let mut path = Config::user_config_dir_path()?;
                    path.push("rust-analyzer.toml");
                    Some(path)
                })();
                let user_config_vfs_path =
                    user_config_path.as_ref().map(|path| VfsPath::from(path.clone()));
                let user_config_text = user_config_vfs_path
                    .as_ref()
                    .and_then(|path| {
                        self.mem_docs.get(path).and_then(|doc| {
                            std::str::from_utf8(&doc.data).ok().map(ToOwned::to_owned)
                        })
                    })
                    .or_else(|| {
                        user_config_path
                            .as_ref()
                            .and_then(|path| fs::read_to_string(path).ok())
                    });
                let shared_source_root_parent_map = Arc::new(shared.source_root_parent_map());
                let source_roots = {
                    let guard = self.vfs.read();
                    self.source_root_config.partition(&guard.0)
                };
                let config_change = self.analyzed_config_change_from_ratoml(
                    modified_ratoml_files,
                    &source_roots,
                    shared_ratoml_files,
                    user_config_text,
                    shared_source_root_parent_map,
                );
                let (config, errors, should_update) = self.config.apply_change(config_change);
                self.config_errors = (!errors.is_empty()).then_some(errors);

                if should_update {
                    self.update_configuration(config);
                } else {
                    self.config = Arc::new(config);
                }
                changed = true;
            }
            if !matches!(&workspace_structure_change, Some((.., true))) {
                let modified_rust_files = self
                    .mem_docs
                    .iter()
                    .filter(|path| {
                        path.as_path()
                            .is_some_and(|path| path.extension() == Some("rs"))
                    })
                    .filter_map(|path| shared.vfs_path_to_file_id(path).ok().flatten())
                    .collect::<Vec<_>>();
                _ = self
                    .deferred_task_queue
                    .sender
                    .send(crate::main_loop::DeferredTask::CheckProcMacroSources(modified_rust_files));
            }

            if let Some((path, force_crate_graph_reload)) = workspace_structure_change {
                self.fetch_workspaces_queue.request_op(
                    "workspace structure changed".to_owned(),
                    FetchWorkspaceRequest { path: Some(path), force_crate_graph_reload },
                );
            }

            changed
        }

        pub(crate) fn analyzed_reload_config_from_shared(&mut self) {
            let shared = &self.analyzed_shared;
            let shared_ratoml_files = shared.ratoml_files();
            let user_config_path = (|| {
                let mut path = Config::user_config_dir_path()?;
                path.push("rust-analyzer.toml");
                Some(path)
            })();
            let user_config_vfs_path = user_config_path.as_ref().map(|path| VfsPath::from(path.clone()));
            let user_config_text = user_config_vfs_path
                .as_ref()
                .and_then(|path| {
                    self.mem_docs.get(path).and_then(|doc| {
                        std::str::from_utf8(&doc.data).ok().map(ToOwned::to_owned)
                    })
                })
                .or_else(|| user_config_path.as_ref().and_then(|path| fs::read_to_string(path).ok()));
            let shared_source_root_parent_map = Arc::new(shared.source_root_parent_map());
            let source_roots = {
                let guard = self.vfs.read();
                self.source_root_config.partition(&guard.0)
            };
            let config_change = self.analyzed_config_change_from_ratoml(
                Vec::new(),
                &source_roots,
                shared_ratoml_files,
                user_config_text,
                shared_source_root_parent_map,
            );
            let (config, errors, should_update) = self.config.apply_change(config_change);
            self.config_errors = (!errors.is_empty()).then_some(errors);

            if should_update {
                self.update_configuration(config);
            } else {
                self.config = Arc::new(config);
            }
        }

        fn analyzed_config_change_from_ratoml(
            &self,
            modified_ratoml_files: Vec<(ChangeKind, VfsPath, Option<String>)>,
            source_roots: &[SourceRoot],
            shared_ratoml_files: Vec<(VfsPath, SourceRootId, bool, String)>,
            user_config_text: Option<String>,
            shared_source_root_parent_map: Arc<FxHashMap<SourceRootId, SourceRootId>>,
        ) -> ConfigChange {
            let user_config_path = (|| {
                let mut path = Config::user_config_dir_path()?;
                path.push("rust-analyzer.toml");
                Some(path)
            })();
            let user_config_abs_path = user_config_path.as_deref();
            let workspace_ratoml_paths = self
                .workspaces
                .iter()
                .map(|workspace| {
                    VfsPath::from({
                        let mut path = workspace.workspace_root().to_owned();
                        path.push("rust-analyzer.toml");
                        path
                    })
                })
                .collect::<Vec<_>>();
            let mut change = ConfigChange::default();
            let mut ratoml_files = shared_ratoml_files
                .into_iter()
                .map(|(path, source_root_id, is_library, text)| {
                    (path, source_root_id, is_library, Some(Arc::<str>::from(text)))
                })
                .collect::<Vec<_>>();
            let mut user_config_changed = false;

            for (_kind, vfs_path, text) in modified_ratoml_files {
                let text = text.map(Arc::<str>::from);
                if vfs_path.as_path() == user_config_abs_path {
                    change.change_user_config(text.clone());
                    user_config_changed = true;
                }

                let Some((source_root_id, source_root)) =
                    source_root_for_path(source_roots, &vfs_path)
                else {
                    continue;
                };
                ratoml_files.push((vfs_path, source_root_id, source_root.is_library, text));
            }

            if !user_config_changed
                && let Some(text) = user_config_text
            {
                change.change_user_config(Some(Arc::<str>::from(text)));
            }

            for (vfs_path, source_root_id, is_library, text) in ratoml_files {
                if is_library {
                    continue;
                }

                let entry = if workspace_ratoml_paths.contains(&vfs_path) {
                    change.change_workspace_ratoml(source_root_id, vfs_path.clone(), text.clone())
                } else {
                    change.change_ratoml(source_root_id, vfs_path.clone(), text.clone())
                };

                if let Some((kind, old_path, old_text)) = entry
                    && old_path < vfs_path
                {
                    match kind {
                        crate::config::RatomlFileKind::Crate => {
                            change.change_ratoml(source_root_id, old_path, old_text);
                        }
                        crate::config::RatomlFileKind::Workspace => {
                            change.change_workspace_ratoml(source_root_id, old_path, old_text);
                        }
                    }
                }
            }

            change.change_source_root_parent_map(shared_source_root_parent_map);
            change
        }

        pub(crate) fn analyzed_base_url_to_file_id(
            &self,
            url: &Url,
        ) -> anyhow::Result<Option<FileId>> {
            self.analyzed_shared.base_url_to_file_id(url)
        }

        pub(crate) fn analyzed_filter_diagnostics(
            &self,
            diagnostics: Vec<lsp_types::Diagnostic>,
        ) -> Vec<lsp_types::Diagnostic> {
            let rustc_diagnostics = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.source.as_deref() == Some("rustc"))
                .map(analyzed_diagnostic_key)
                .collect::<Vec<_>>();

            diagnostics
                .into_iter()
                .filter(|diagnostic| {
                    diagnostic.source.as_deref() != Some("rust-analyzer")
                        || !rustc_diagnostics
                            .iter()
                            .any(|key| *key == analyzed_diagnostic_key(diagnostic))
                })
                .collect()
        }
    }

    fn analyzed_diagnostic_key(
        diagnostic: &lsp_types::Diagnostic,
    ) -> (lsp_types::Range, Option<String>) {
        let code = diagnostic.code.as_ref().map(|code| match code {
            lsp_types::NumberOrString::Number(code) => code.to_string(),
            lsp_types::NumberOrString::String(code) => code.clone(),
        });

        (diagnostic.range, code)
    }

    fn source_root_for_path<'a>(
        source_roots: &'a [SourceRoot],
        path: &VfsPath,
    ) -> Option<(SourceRootId, &'a SourceRoot)> {
        source_roots.iter().enumerate().find_map(|(index, source_root)| {
            source_root
                .file_for_path(path)
                .map(|_| (SourceRootId(index as u32), source_root))
        })
    }

    fn config_from_initialize_params(
        connection: &Connection,
        initialize_params: &serde_json::Value,
    ) -> anyhow::Result<Config> {
        let lsp_types::InitializeParams {
            root_uri,
            mut capabilities,
            workspace_folders,
            initialization_options,
            client_info,
            ..
        } = from_json::<lsp_types::InitializeParams>("InitializeParams", initialize_params)?;

        if let Some(value) = initialize_params.pointer("/capabilities/workspace/diagnostics")
            && let Ok(diagnostics) =
                from_json::<lsp_types::DiagnosticWorkspaceClientCapabilities>(
                    "DiagnosticWorkspaceClientCapabilities",
                    value,
                )
        {
            capabilities.workspace.get_or_insert_default().diagnostic.get_or_insert(diagnostics);
        }

        let root_path = match root_uri
            .and_then(|it| it.to_file_path().ok())
            .map(patch_path_prefix)
            .and_then(|it| Utf8PathBuf::from_path_buf(it).ok())
            .and_then(|it| AbsPathBuf::try_from(it).ok())
        {
            Some(it) => it,
            None => AbsPathBuf::assert_utf8(env::current_dir()?),
        };

        if let Some(client_info) = &client_info {
            tracing::info!(
                "Client '{}' {}",
                client_info.name,
                client_info.version.as_deref().unwrap_or_default()
            );
        }

        let workspace_roots = workspace_folders
            .map(|workspaces| {
                workspaces
                    .into_iter()
                    .filter_map(|it| it.uri.to_file_path().ok())
                    .map(patch_path_prefix)
                    .filter_map(|it| Utf8PathBuf::from_path_buf(it).ok())
                    .filter_map(|it| AbsPathBuf::try_from(it).ok())
                    .collect::<Vec<_>>()
            })
            .filter(|workspaces| !workspaces.is_empty())
            .unwrap_or_else(|| vec![root_path.clone()]);
        let mut config = Config::new(root_path, capabilities, workspace_roots, client_info);

        if let Some(json) = initialization_options {
            let mut change = ConfigChange::default();
            change.change_client_config(json);

            let errors: ConfigErrors;
            (config, errors, _) = config.apply_change(change);

            if !errors.is_empty() {
                let notification = lsp_server::Notification::new(
                    <lsp_types::notification::ShowMessage as lsp_types::notification::Notification>::METHOD.to_owned(),
                    lsp_types::ShowMessageParams {
                        typ: lsp_types::MessageType::WARNING,
                        message: errors.to_string(),
                    },
                );
                connection.sender.send(lsp_server::Message::Notification(notification))?;
            }
        }

        Ok(config)
    }

    fn initialize_rayon() {
        static RAYON: Once = Once::new();

        RAYON.call_once(|| {
            _ = rayon::ThreadPoolBuilder::new()
                .thread_name(|index| format!("RayonWorker{index}"))
                .build_global();
        });
    }
}
"#;
