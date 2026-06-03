use std::{
    env,
    error::Error,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Command,
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use toml::{Table, Value, map::Map};

const RA_PACKAGE: &str = "ra_ap_rust-analyzer";

#[derive(Debug)]
struct LockedPackage {
    version: String,
    checksum: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let package = locked_ra_package()?;
    let archive = registry_archive(&package)?;
    verify_archive_checksum(&archive, &package)?;
    let generated = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("ra_ap_rust_analyzer_bridge");
    unpack_crate_archive(&archive, &generated, &package)?;
    verify_manifest_matches_bridge(&generated.join("Cargo.toml"))?;
    let generated_src = generated.join("src");

    rewrite_lib_header(&generated_src.join("lib.rs"))?;
    write_bridge_module(&generated_src.join("analyzed_bridge.rs"))?;
    append_main_loop_session_module(&generated_src.join("main_loop.rs"))?;
    append_bridge_export(&generated_src.join("lib.rs"))?;

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", archive.display());
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root().join("Cargo.lock").display()
    );

    Ok(())
}

fn registry_archive(package: &LockedPackage) -> Result<PathBuf, Box<dyn Error>> {
    let cargo_home = cargo_home()?;
    if let Some(archive) = find_registry_archive(&cargo_home, package)? {
        return Ok(archive);
    }

    fetch_registry_archive()?;

    if let Some(archive) = find_registry_archive(&cargo_home, package)? {
        return Ok(archive);
    }

    Err(format!(
        "could not find {} under {} after `cargo fetch --locked`",
        archive_name(package),
        cargo_home.join("registry").join("cache").display()
    )
    .into())
}

fn cargo_home() -> Result<PathBuf, Box<dyn Error>> {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or_else(|| "CARGO_HOME is unavailable".into())
}

fn find_registry_archive(
    cargo_home: &Path,
    package: &LockedPackage,
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let registry_cache = cargo_home.join("registry").join("cache");
    let package_archive = archive_name(package);
    let registries = fs::read_dir(&registry_cache)?;

    for registry in registries {
        let candidate = registry?.path().join(&package_archive);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn fetch_registry_archive() -> Result<(), Box<dyn Error>> {
    let cargo = env::var_os("CARGO").ok_or("CARGO is unavailable")?;
    let status = Command::new(cargo)
        .arg("fetch")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(workspace_root().join("Cargo.toml"))
        .status()?;

    if !status.success() {
        return Err(format!("cargo fetch --locked failed with {status}").into());
    }

    Ok(())
}

fn verify_archive_checksum(archive: &Path, package: &LockedPackage) -> Result<(), Box<dyn Error>> {
    let actual = hex_digest(fs::read(archive)?);

    if actual != package.checksum {
        return Err(format!(
            "checksum mismatch for {}: expected {}, got {actual}",
            archive.display(),
            package.checksum,
        )
        .into());
    }

    Ok(())
}

fn locked_ra_package() -> Result<LockedPackage, Box<dyn Error>> {
    let lock = fs::read_to_string(workspace_root().join("Cargo.lock"))?;
    let mut packages = Vec::new();
    let mut name = None;
    let mut version = None;
    let mut checksum = None;

    for line in lock.lines() {
        if line == "[[package]]" {
            push_locked_ra_package(&mut packages, &mut name, &mut version, &mut checksum)?;
            continue;
        }

        if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("version = ") {
            version = Some(value.trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("checksum = ") {
            checksum = Some(value.trim_matches('"').to_owned());
        }
    }

    push_locked_ra_package(&mut packages, &mut name, &mut version, &mut checksum)?;

    match packages.len() {
        1 => Ok(packages.remove(0)),
        0 => Err(format!("could not find {RA_PACKAGE} in Cargo.lock").into()),
        count => Err(format!("found {count} {RA_PACKAGE} packages in Cargo.lock").into()),
    }
}

fn push_locked_ra_package(
    packages: &mut Vec<LockedPackage>,
    name: &mut Option<String>,
    version: &mut Option<String>,
    checksum: &mut Option<String>,
) -> Result<(), Box<dyn Error>> {
    if name.as_deref() == Some(RA_PACKAGE) {
        packages.push(LockedPackage {
            version: version
                .take()
                .ok_or_else(|| format!("{RA_PACKAGE} is missing version in Cargo.lock"))?,
            checksum: checksum
                .take()
                .ok_or_else(|| format!("{RA_PACKAGE} is missing checksum in Cargo.lock"))?,
        });
    }

    *name = None;
    *version = None;
    *checksum = None;

    Ok(())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unpack_crate_archive(
    archive: &Path,
    destination: &Path,
    package: &LockedPackage,
) -> Result<(), Box<dyn Error>> {
    let decoder = GzDecoder::new(fs::File::open(archive)?);
    let mut archive = tar::Archive::new(decoder);
    let package_dir = package_dir(package);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let relative = checked_archive_path(&path, &package_dir)?;
        let destination_path = destination.join(relative);
        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            fs::create_dir_all(&destination_path)?;
        } else if entry_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            entry.unpack(&destination_path)?;
        } else {
            return Err(format!("unsupported archive entry type for {}", path.display()).into());
        }
    }

    Ok(())
}

fn checked_archive_path(path: &Path, package_dir: &str) -> Result<PathBuf, Box<dyn Error>> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(component)) if component == package_dir => {}
        _ => return Err(format!("unexpected archive path {}", path.display()).into()),
    }

    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative.push(part),
            _ => return Err(format!("unsupported archive path {}", path.display()).into()),
        }
    }

    if relative.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(relative)
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("../..")
}

fn package_dir(package: &LockedPackage) -> String {
    format!("{RA_PACKAGE}-{}", package.version)
}

fn archive_name(package: &LockedPackage) -> String {
    format!("{}.crate", package_dir(package))
}

fn append_bridge_export(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut file = fs::OpenOptions::new().append(true).open(lib_rs)?;
    file.write_all(
        br#"

pub mod analyzed_bridge;
pub use analyzed_bridge::{
    BackendCore, LoadedWorkspace, RustAnalyzerLspBoundary,
    RustAnalyzerPrivateBoundary, RustAnalyzerSession, RustAnalyzerSessionBoundary, WorkspaceSummary,
    run_rust_analyzer_lsp_session, rust_analyzer_lsp_boundary, rust_analyzer_private_boundary,
    rust_analyzer_session_boundary,
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
            "analyzed-ra Cargo.toml is out of sync with {RA_PACKAGE}:\n{}",
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
        (Some(_), None) => mismatches.push(format!("  {label}: missing section in analyzed-ra")),
        (None, Some(_)) => mismatches.push(format!("  {label}: extra section in analyzed-ra")),
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
            "  {label}: missing keys in analyzed-ra: {}",
            expected_only.join(", ")
        ));
    }
    if !actual_only.is_empty() {
        mismatches.push(format!(
            "  {label}: extra keys in analyzed-ra: {}",
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
        Value::Table(dependency) => Value::Table(dependency.clone()),
        _ => value.clone(),
    }
}

fn table_keys(table: &Table) -> Vec<String> {
    let mut keys = table.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn rewrite_lib_header(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let source = fs::read_to_string(lib_rs)?;
    let mut rewritten = String::new();

    for line in source.lines() {
        if line.starts_with("//!") || line.starts_with("#![") {
            continue;
        }

        rewritten.push_str(line);
        rewritten.push('\n');
    }

    fs::write(lib_rs, rewritten)?;
    Ok(())
}

fn write_bridge_module(path: &Path) -> Result<(), Box<dyn Error>> {
    fs::write(path, BRIDGE_MODULE)?;
    Ok(())
}

const BRIDGE_MODULE: &str = r#"
use std::path::Path;

use ide::{AnalysisHost, RootDatabase};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_into_db};
use proc_macro_api::ProcMacroClient;
use project_model::{CargoConfig, ProjectManifest, ProjectWorkspace};
use serde::Serialize;
use vfs::{AbsPathBuf, Vfs};

pub use crate::main_loop::analyzed_session::RustAnalyzerSession;

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

#[derive(Clone, Debug, Serialize)]
pub struct RustAnalyzerSessionBoundary {
    pub session: &'static str,
    pub runner: &'static str,
}

pub fn rust_analyzer_session_boundary() -> RustAnalyzerSessionBoundary {
    let _session_size = std::mem::size_of::<RustAnalyzerSession>();
    let _runner = RustAnalyzerSession::run_connection;
    let _lsp_runner = run_rust_analyzer_lsp_session;

    RustAnalyzerSessionBoundary {
        session: std::any::type_name::<RustAnalyzerSession>(),
        runner: "ra_ap_rust_analyzer::main_loop::analyzed_session::RustAnalyzerSession::run_connection",
    }
}

pub fn run_rust_analyzer_lsp_session(connection: lsp_server::Connection) -> anyhow::Result<()> {
    crate::main_loop::analyzed_session::run_lsp_session(connection)
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSummary {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
    pub files: usize,
    pub proc_macro_server: bool,
}

pub struct LoadedWorkspace {
    summary: WorkspaceSummary,
    _vfs: Vfs,
    _proc_macro_client: Option<ProcMacroClient>,
}

impl LoadedWorkspace {
    pub fn summary(&self) -> &WorkspaceSummary {
        &self.summary
    }
}

pub struct BackendCore {
    _host: AnalysisHost,
    loaded_workspaces: Vec<LoadedWorkspace>,
}

impl BackendCore {
    pub fn new() -> Self {
        Self {
            _host: AnalysisHost::with_database(RootDatabase::new(None)),
            loaded_workspaces: Vec::new(),
        }
    }

    pub fn load_workspace_roots<I, P>(roots: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut core = Self::new();

        for root in roots {
            core.load_cargo_workspace(root)?;
        }

        Ok(core)
    }

    pub fn load_cargo_workspace(
        &mut self,
        root: impl AsRef<Path>,
    ) -> anyhow::Result<&WorkspaceSummary> {
        let loaded = load_cargo_workspace_into_host(&mut self._host, root)?;
        self.loaded_workspaces.push(loaded);

        Ok(self
            .loaded_workspaces
            .last()
            .expect("workspace was just inserted")
            .summary())
    }

    pub fn workspace_summaries(&self) -> impl Iterator<Item = &WorkspaceSummary> {
        self.loaded_workspaces.iter().map(LoadedWorkspace::summary)
    }
}

impl Default for BackendCore {
    fn default() -> Self {
        Self::new()
    }
}

fn load_cargo_workspace_into_host(
    host: &mut AnalysisHost,
    root: impl AsRef<Path>,
) -> anyhow::Result<LoadedWorkspace> {
    let cargo_config = CargoConfig::default();
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: false,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let root = AbsPathBuf::assert_utf8(std::fs::canonicalize(root)?);
    let manifest = ProjectManifest::discover_single(&root)?;
    let manifest_path = manifest.manifest_path().clone();
    let workspace = ProjectWorkspace::load(manifest, &cargo_config, &|_| {})?;
    let root = workspace.workspace_root().to_string();
    let packages = workspace.n_packages();
    let (vfs, proc_macro_client) = {
        let db = host.raw_database_mut();
        load_workspace_into_db(workspace, &cargo_config.extra_env, &load_config, db)?
    };
    let files = vfs.iter().count();

    Ok(LoadedWorkspace {
        summary: WorkspaceSummary {
            root,
            manifest: manifest_path.to_string(),
            packages,
            files,
            proc_macro_server: proc_macro_client.is_some(),
        },
        _vfs: vfs,
        _proc_macro_client: proc_macro_client,
    })
}
"#;

const MAIN_LOOP_SESSION_MODULE: &str = r#"

pub(crate) mod analyzed_session {
    use std::{env, path::PathBuf, sync::Once};

    use crossbeam_channel::{Receiver, Sender};
    use lsp_server::{Connection, Message};
    use paths::Utf8PathBuf;
    use vfs::AbsPathBuf;

    use crate::{
        config::{Config, ConfigChange, ConfigErrors},
        from_json, server_capabilities, version,
    };

    pub struct RustAnalyzerSession {
        state: crate::global_state::GlobalState,
    }

    impl RustAnalyzerSession {
        pub fn new(sender: Sender<Message>, config: crate::config::Config) -> Self {
            Self {
                state: crate::global_state::GlobalState::new(sender, config),
            }
        }

        pub fn run(self, receiver: Receiver<Message>) -> anyhow::Result<()> {
            self.state.run(receiver)
        }

        pub fn run_connection(
            config: crate::config::Config,
            connection: Connection,
        ) -> anyhow::Result<()> {
            Self::new(connection.sender, config).run(connection.receiver)
        }
    }

    pub(crate) fn run_lsp_session(connection: Connection) -> anyhow::Result<()> {
        let (initialize_id, initialize_params) = connection.initialize_start()?;
        tracing::info!("InitializeParams: {}", initialize_params);
        let mut config = config_from_initialize_params(&connection, &initialize_params)?;
        let initialize_result = lsp_types::InitializeResult {
            capabilities: server_capabilities(&config),
            server_info: Some(lsp_types::ServerInfo {
                name: String::from("rust-analyzer"),
                version: Some(version().to_string()),
            }),
            offset_encoding: None,
        };

        connection.initialize_finish(initialize_id, serde_json::to_value(initialize_result)?)?;

        if config.discover_workspace_config().is_none()
            && !config.has_linked_projects()
            && config.detached_files().is_empty()
        {
            config.rediscover_workspaces();
        }

        initialize_rayon();
        let Connection { sender, receiver } = connection;
        RustAnalyzerSession::new(sender, config).run(receiver)
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

    fn patch_path_prefix(path: PathBuf) -> PathBuf {
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
}
"#;
