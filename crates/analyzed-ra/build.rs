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
    patch_global_state_source(&generated_src.join("global_state.rs"))?;
    patch_main_loop_source(&generated_src.join("main_loop.rs"))?;
    write_bridge_module(&generated_src.join("analyzed_bridge.rs"), &package.version)?;
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
	    PackageInstance, PackageInstanceKey, RustAnalyzerLspBoundary, RustAnalyzerPrivateBoundary,
	    SessionOverlay, SessionOverlayCrate, SessionOverlayFile, SharedAnalyzerBackendKey,
	    SharedAnalyzerCargoConfigKey, SharedAnalyzerConfig,
	    SharedAnalyzerLoadKey, SharedAnalyzerProcMacroServerKey, SharedAnalyzerSession,
	    SharedAnalyzerSessionContext, SharedAnalyzerWorldConfigKey, SharedAnalyzerWorldKey,
	    SharedAnalyzerViewKey, SharedWorld, WorkspaceSummary, WorkspaceView,
	    run_shared_rust_analyzer_lsp_session, rust_analyzer_lsp_boundary,
	    rust_analyzer_private_boundary, shared_analyzer_session_context,
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

fn patch_global_state_source(global_state_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(global_state_rs)?;

    replace_once(
        &mut source,
        "    pub(crate) analysis_host: AnalysisHost,\n    pub(crate) diagnostics: DiagnosticCollection,\n",
        "    pub(crate) analysis_host: AnalysisHost,\n    pub(crate) analyzed_shared: Option<crate::analyzed_bridge::SharedAnalyzerRuntime>,\n    pub(crate) diagnostics: DiagnosticCollection,\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) analysis: Analysis,\n    pub(crate) check_fixes: CheckFixes,\n",
        "    pub(crate) analysis: Analysis,\n    pub(crate) analyzed_shared: Option<crate::analyzed_bridge::SharedAnalyzerRuntime>,\n    pub(crate) check_fixes: CheckFixes,\n",
    )?;
    replace_once(
        &mut source,
        "            analysis_host,\n            diagnostics: Default::default(),\n",
        "            analysis_host,\n            analyzed_shared: None,\n            diagnostics: Default::default(),\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn process_changes(&mut self) -> bool {\n        let _p = span!(Level::INFO, \"GlobalState::process_changes\").entered();\n",
        "    pub(crate) fn process_changes(&mut self) -> bool {\n        let _p = span!(Level::INFO, \"GlobalState::process_changes\").entered();\n        if self.analyzed_shared.is_some() {\n            return self.analyzed_process_shared_changes();\n        }\n",
    )?;
    replace_once(
        &mut source,
        "            workspaces: Arc::clone(&self.workspaces),\n            analysis: self.analysis_host.analysis(),\n            vfs: Arc::clone(&self.vfs),\n",
        "            workspaces: Arc::clone(&self.workspaces),\n            analysis: self\n                .analyzed_shared\n                .as_ref()\n                .map_or_else(|| self.analysis_host.analysis(), |shared| shared.analysis()),\n            analyzed_shared: self.analyzed_shared.clone(),\n            vfs: Arc::clone(&self.vfs),\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {\n        url_to_file_id(&self.vfs_read(), url)\n    }\n",
        "    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {\n        if let Some(shared) = &self.analyzed_shared {\n            return shared.url_to_file_id(url);\n        }\n        url_to_file_id(&self.vfs_read(), url)\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {\n        file_id_to_url(&self.vfs_read(), id)\n    }\n",
        "    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {\n        if let Some(shared) = &self.analyzed_shared\n            && let Some(url) = shared.file_id_to_url(id)\n        {\n            return url;\n        }\n        file_id_to_url(&self.vfs_read(), id)\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {\n        vfs_path_to_file_id(&self.vfs_read(), vfs_path)\n    }\n",
        "    pub(crate) fn vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {\n        if let Some(shared) = &self.analyzed_shared {\n            return shared.vfs_path_to_file_id(vfs_path);\n        }\n        vfs_path_to_file_id(&self.vfs_read(), vfs_path)\n    }\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {\n        let endings = self.vfs.read().1[&file_id];\n        let index = self.analysis.file_line_index(file_id)?;\n",
        "    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {\n        let endings = self\n            .analyzed_shared\n            .as_ref()\n            .and_then(|shared| shared.line_endings(file_id))\n            .unwrap_or_else(|| self.vfs.read().1[&file_id]);\n        let index = self.analysis.file_line_index(file_id)?;\n",
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
        "    pub(crate) fn file_id_to_file_path(&self, file_id: FileId) -> vfs::VfsPath {\n        if let Some(shared) = &self.analyzed_shared\n            && let Some(path) = shared.file_id_to_vfs_path(file_id)\n        {\n            return path;\n        }\n        self.vfs_read().file_path(file_id).clone()\n    }\n",
    )?;
    replace_once(
        &mut source,
        "        let path = self.vfs_read().file_path(file_id).clone();\n        let path = path.as_path()?;\n",
        "        let path = self.file_id_to_file_path(file_id);\n        let path = path.as_path()?;\n",
    )?;
    replace_once(
        &mut source,
        "    pub(crate) fn file_exists(&self, file_id: FileId) -> bool {\n        self.vfs.read().0.exists(file_id)\n    }\n",
        "    pub(crate) fn file_exists(&self, file_id: FileId) -> bool {\n        if let Some(shared) = &self.analyzed_shared\n            && let Some(exists) = shared.file_exists(file_id)\n        {\n            return exists;\n        }\n        self.vfs.read().0.exists(file_id)\n    }\n",
    )?;

    fs::write(global_state_rs, source)?;
    Ok(())
}

fn patch_main_loop_source(main_loop_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(main_loop_rs)?;

    replace_once(
        &mut source,
        "    global_state::{\n        FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState,\n        file_id_to_url, url_to_file_id,\n    },\n",
        "    global_state::{\n        FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState,\n    },\n",
    )?;
    replace_once(
        &mut source,
        "                let uri = file_id_to_url(&self.vfs.read().0, file_id);\n",
        "                let uri = self.analyzed_file_id_to_url(file_id);\n",
    )?;
    replace_once(
        &mut source,
        "                    match url_to_file_id(&self.vfs.read().0, &diag.url) {\n",
        "                    match self.analyzed_url_to_file_id(&diag.url) {\n",
    )?;
    replace_once(
        &mut source,
        "        let db = self.analysis_host.raw_database();\n        let generation = self.diagnostics.next_generation();\n        let subscriptions = {\n            let vfs = &self.vfs.read().0;\n            self.mem_docs\n                .iter()\n                .map(|path| vfs.file_id(path).unwrap())\n                .filter_map(|(file_id, excluded)| {\n                    (excluded == vfs::FileExcluded::No).then_some(file_id)\n                })\n                .filter(|&file_id| {\n                    let source_root_id = db.file_source_root(file_id).source_root_id(db);\n                    let source_root = db.source_root(source_root_id).source_root(db);\n                    // Only publish diagnostics for files in the workspace, not from crates.io deps\n                    // or the sysroot.\n                    // While theoretically these should never have errors, we have quite a few false\n                    // positives particularly in the stdlib, and those diagnostics would stay around\n                    // forever if we emitted them here.\n                    !source_root.is_library\n                })\n                .collect::<std::sync::Arc<_>>()\n        };\n",
        "        let generation = self.diagnostics.next_generation();\n        let subscriptions = if self.analyzed_shared.is_some() {\n            let snap = self.snapshot();\n            self.mem_docs\n                .iter()\n                .filter_map(|path| snap.vfs_path_to_file_id(path).ok().flatten())\n                .filter(|&file_id| {\n                    snap.analysis\n                        .is_library_file(file_id)\n                        .is_ok_and(|is_library| !is_library)\n                })\n                .collect::<std::sync::Arc<_>>()\n        } else {\n            let db = self.analysis_host.raw_database();\n            let vfs = &self.vfs.read().0;\n            self.mem_docs\n                .iter()\n                .map(|path| vfs.file_id(path).unwrap())\n                .filter_map(|(file_id, excluded)| {\n                    (excluded == vfs::FileExcluded::No).then_some(file_id)\n                })\n                .filter(|&file_id| {\n                    let source_root_id = db.file_source_root(file_id).source_root_id(db);\n                    let source_root = db.source_root(source_root_id).source_root(db);\n                    // Only publish diagnostics for files in the workspace, not from crates.io deps\n                    // or the sysroot.\n                    // While theoretically these should never have errors, we have quite a few false\n                    // positives particularly in the stdlib, and those diagnostics would stay around\n                    // forever if we emitted them here.\n                    !source_root.is_library\n                })\n                .collect::<std::sync::Arc<_>>()\n        };\n",
    )?;

    fs::write(main_loop_rs, source)?;
    Ok(())
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
	    collections::{BTreeMap, btree_map::Entry},
	    env,
	    path::{Path, PathBuf},
	    sync::{Arc, Mutex},
	};

	use hir::ChangeWithProcMacros;
	use ide::{Analysis, AnalysisHost, FileId, RootDatabase};
	use ide_db::base_db::{SourceDatabase, all_crates};
	use load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_into_db};
	use lsp_types::Url;
	use proc_macro_api::ProcMacroClient;
	use project_model::{CargoConfig, ProjectManifest, ProjectWorkspace};
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
    session: SharedAnalyzerSession,
) -> anyhow::Result<()> {
    crate::main_loop::analyzed_session::run_shared_lsp_session(connection, session)
}

#[derive(Clone, Debug)]
pub struct SharedAnalyzerBackendKey {
    pub shared_world: SharedAnalyzerWorldKey,
    pub workspace_view: SharedAnalyzerViewKey,
}

#[derive(Clone, Debug)]
pub struct SharedAnalyzerWorldKey {
    pub rust_analyzer_version: String,
    pub toolchain: Option<String>,
    pub sysroot: Option<String>,
    pub cargo_target: Option<String>,
    pub config: SharedAnalyzerWorldConfigKey,
    pub load: SharedAnalyzerLoadKey,
}

#[derive(Clone, Debug)]
pub struct SharedAnalyzerWorldConfigKey {
    pub cargo: SharedAnalyzerCargoConfigKey,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct SharedAnalyzerLoadKey {
    pub load_out_dirs_from_check: bool,
    pub proc_macro_server: SharedAnalyzerProcMacroServerKey,
    pub prefill_caches: bool,
    pub num_worker_threads: u16,
    pub proc_macro_processes: u16,
}

#[derive(Clone, Debug)]
pub enum SharedAnalyzerProcMacroServerKey {
    None,
    Sysroot,
    Explicit(String),
}

#[derive(Clone, Debug)]
pub struct SharedAnalyzerViewKey {
    pub workspace_roots: Vec<String>,
    pub analysis: SharedAnalyzerAnalysisKey,
}

#[derive(Clone, Debug)]
pub struct SharedAnalyzerAnalysisKey {
    pub initialization_options: Option<String>,
    pub workspace_configuration: Option<String>,
}

pub fn shared_analyzer_session_context(
    initialize_params: &serde_json::Value,
) -> anyhow::Result<SharedAnalyzerSessionContext> {
    let lsp_types::InitializeParams {
        root_uri,
        capabilities,
        workspace_folders,
        initialization_options,
        client_info,
        ..
    } = crate::from_json::<lsp_types::InitializeParams>("InitializeParams", initialize_params)?;
    let root_path = root_path_from_initialize(root_uri)?;
    let workspace_roots = workspace_roots_from_initialize(&root_path, workspace_folders)?;
    let mut config = crate::config::Config::new(
        root_path,
        capabilities,
        workspace_roots
            .iter()
            .filter_map(|root| AbsPathBuf::try_from(root.as_str()).ok())
            .collect(),
        client_info,
    );

    if let Some(json) = initialization_options.clone() {
        let mut change = crate::config::ConfigChange::default();
        change.change_client_config(json);

        let errors: crate::config::ConfigErrors;
        (config, errors, _) = config.apply_change(change);
        if !errors.is_empty() {
            tracing::warn!("rust-analyzer config errors while deriving backend key: {errors}");
        }
    }

    if config.discover_workspace_config().is_none()
        && !config.has_linked_projects()
        && config.detached_files().is_empty()
    {
        config.rediscover_workspaces();
    }

    let cargo_config = config.cargo(None);
    let load = shared_load_config_from_config(&config)?;
    let analysis = SharedAnalyzerAnalysisKey {
        initialization_options: initialization_options
            .as_ref()
            .map(canonical_json_string)
            .transpose()?,
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
            analysis,
        },
    };

    Ok(SharedAnalyzerSessionContext {
        backend_key,
        config: Arc::new(SharedAnalyzerConfig {
            workspace_roots,
            cargo_config,
            load,
        }),
    })
}

pub struct SharedAnalyzerSessionContext {
    pub backend_key: SharedAnalyzerBackendKey,
    pub config: Arc<SharedAnalyzerConfig>,
}

pub struct SharedAnalyzerConfig {
    workspace_roots: Vec<String>,
    pub(crate) cargo_config: CargoConfig,
    pub(crate) load: SharedLoadConfig,
}

impl SharedAnalyzerConfig {
    pub fn workspace_roots(&self) -> &[String] {
        &self.workspace_roots
    }
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

fn root_path_from_initialize(root_uri: Option<lsp_types::Url>) -> anyhow::Result<AbsPathBuf> {
    match root_uri
        .and_then(|it| it.to_file_path().ok())
        .map(patch_path_prefix)
        .and_then(|it| std::fs::canonicalize(it).ok())
        .and_then(|it| paths::Utf8PathBuf::from_path_buf(it).ok())
        .and_then(|it| AbsPathBuf::try_from(it).ok())
    {
        Some(path) => Ok(path),
        None => Ok(AbsPathBuf::assert_utf8(std::fs::canonicalize(env::current_dir()?)?)),
    }
}

fn workspace_roots_from_initialize(
    root_path: &AbsPathBuf,
    workspace_folders: Option<Vec<lsp_types::WorkspaceFolder>>,
) -> anyhow::Result<Vec<String>> {
    let mut roots = workspace_folders
        .unwrap_or_default()
        .into_iter()
        .filter_map(|workspace| workspace.uri.to_file_path().ok())
        .map(patch_path_prefix)
        .map(std::fs::canonicalize)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    if roots.is_empty() {
        roots.push(root_path.to_string());
    }

    roots.sort();
    roots.dedup();

    Ok(roots)
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

fn canonical_json_string(value: &serde_json::Value) -> anyhow::Result<String> {
    let mut output = String::new();
    write_canonical_json(value, &mut output)?;
    Ok(output)
}

fn write_canonical_json(value: &serde_json::Value, output: &mut String) -> anyhow::Result<()> {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => output.push_str(&serde_json::to_string(value)?),
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            for (index, (key, value)) in
                values.iter().collect::<BTreeMap<_, _>>().iter().enumerate()
            {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key)?);
                output.push(':');
                write_canonical_json(value, output)?;
            }
            output.push('}');
        }
    }

    Ok(())
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
        let runtime = SharedAnalyzerRuntime::new(Arc::clone(&world));

        Self {
            world,
            view,
            runtime,
        }
    }

    pub fn snapshot(&self) -> anyhow::Result<SharedAnalyzerSnapshot> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;

        world.snapshot(&self.view, self.runtime.clone())
    }
}

pub struct SharedAnalyzerSnapshot {
    pub(crate) workspaces: Vec<ProjectWorkspace>,
    pub(crate) runtime: SharedAnalyzerRuntime,
}

#[derive(Clone)]
pub struct SharedAnalyzerRuntime {
    world: Arc<Mutex<SharedWorld>>,
}

impl SharedAnalyzerRuntime {
    fn new(world: Arc<Mutex<SharedWorld>>) -> Self {
        Self { world }
    }

    pub(crate) fn analysis(&self) -> Analysis {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .host
            .analysis()
    }

    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        let path = crate::lsp::from_proto::vfs_path(url)?;
        self.vfs_path_to_file_id(&path)
    }

    pub(crate) fn vfs_path_to_file_id(&self, path: &VfsPath) -> anyhow::Result<Option<FileId>> {
        let path = normalize_vfs_path(path);
        Ok(self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
            .file_id_for_vfs_path(&path))
    }

    pub(crate) fn file_id_to_url(&self, file_id: FileId) -> Option<Url> {
        let path = self.file_id_to_vfs_path(file_id)?;
        let path = path.as_path()?;
        Some(crate::lsp::to_proto::url_from_abs_path(path))
    }

    pub(crate) fn file_id_to_vfs_path(&self, file_id: FileId) -> Option<VfsPath> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .vfs_path_for_file_id(file_id)
    }

    pub(crate) fn line_endings(&self, file_id: FileId) -> Option<crate::line_index::LineEndings> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .line_endings_for_file_id(file_id)
    }

    pub(crate) fn file_exists(&self, file_id: FileId) -> Option<bool> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .file_exists(file_id)
    }

    pub(crate) fn apply_file_change(
        &self,
        path: &VfsPath,
        text: Option<(String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Option<FileId>> {
        let path = normalize_vfs_path(path);
        self.world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
            .apply_file_change(&path, text)
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

    pub fn materialize_files(&mut self, first_file_id: FileId) {
        let mut next_file_id = first_file_id.index();

        for file in &mut self.files {
            if file.session_file.is_none() {
                file.session_file = Some(FileId::from_raw(next_file_id));
                next_file_id += 1;
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

struct LoadedWorkspace {
    summary: WorkspaceSummary,
    workspace: ProjectWorkspace,
    _vfs: Vfs,
    line_endings: BTreeMap<FileId, crate::line_index::LineEndings>,
    _proc_macro_client: Option<ProcMacroClient>,
}

impl LoadedWorkspace {
    fn summary(&self) -> &WorkspaceSummary {
        &self.summary
    }
}

pub struct SharedWorld {
    host: AnalysisHost,
    loaded_workspaces: Vec<LoadedWorkspace>,
    workspace_indexes: BTreeMap<String, usize>,
    package_instances: BTreeMap<PackageInstanceKey, PackageInstance>,
}

impl SharedWorld {
    pub fn new() -> Self {
        Self {
            host: AnalysisHost::with_database(RootDatabase::new(None)),
            loaded_workspaces: Vec::new(),
            workspace_indexes: BTreeMap::new(),
            package_instances: BTreeMap::new(),
        }
    }

    pub fn load_cargo_workspace(
        &mut self,
        root: impl AsRef<Path>,
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<usize> {
        let root = AbsPathBuf::assert_utf8(std::fs::canonicalize(root)?);
        let root_key = root.to_string();
        if let Some(index) = self.workspace_indexes.get(&root_key) {
            return Ok(*index);
        }

        let loaded = load_cargo_workspace_into_host(&mut self.host, root, config)?;
        let index = self.loaded_workspaces.len();
        self.loaded_workspaces.push(loaded);
        self.workspace_indexes.insert(root_key, index);
        self.refresh_package_instances()?;

        Ok(index)
    }

    pub fn workspace_summary(&self, index: usize) -> Option<&WorkspaceSummary> {
        self.loaded_workspaces.get(index).map(LoadedWorkspace::summary)
    }

    pub fn snapshot(
        &self,
        view: &WorkspaceView,
        runtime: SharedAnalyzerRuntime,
    ) -> anyhow::Result<SharedAnalyzerSnapshot> {
        let mut workspaces = Vec::new();

        for index in view.workspace_indexes() {
            let Some(workspace) = self.loaded_workspaces.get(index) else {
                continue;
            };
            workspaces.push(workspace.workspace.clone());
        }

        Ok(SharedAnalyzerSnapshot {
            workspaces,
            runtime,
        })
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

    pub fn file_id_for_vfs_path(&self, path: &VfsPath) -> Option<FileId> {
        self.loaded_workspaces
            .iter()
            .find_map(|workspace| workspace._vfs.file_id(path).map(|(file_id, _)| file_id))
    }

    pub fn vfs_path_for_file_id(&self, file_id: FileId) -> Option<VfsPath> {
        self.loaded_workspaces
            .iter()
            .find_map(|workspace| workspace._vfs.iter().find_map(|(id, path)| (id == file_id).then(|| path.clone())))
    }

    pub(crate) fn line_endings_for_file_id(
        &self,
        file_id: FileId,
    ) -> Option<crate::line_index::LineEndings> {
        self.loaded_workspaces
            .iter()
            .find_map(|workspace| workspace.line_endings.get(&file_id).copied())
    }

    pub fn file_exists(&self, file_id: FileId) -> Option<bool> {
        self.loaded_workspaces.iter().find_map(|workspace| {
            workspace
                ._vfs
                .iter()
                .any(|(id, _)| id == file_id)
                .then(|| workspace._vfs.exists(file_id))
        })
    }

    pub(crate) fn apply_file_change(
        &mut self,
        path: &VfsPath,
        text: Option<(String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Option<FileId>> {
        let Some(file_id) = self.file_id_for_vfs_path(path) else {
            return Ok(None);
        };

        let mut change = ChangeWithProcMacros::default();
        let text = text.map(|(text, line_endings)| {
            for workspace in &mut self.loaded_workspaces {
                if let Entry::Occupied(mut entry) = workspace.line_endings.entry(file_id) {
                    entry.insert(line_endings);
                    break;
                }
            }
            text
        });
        change.change_file(file_id, text);
        self.host.apply_change(change);

        Ok(Some(file_id))
    }

    pub fn crate_root_file(&self, krate: ide::Crate) -> anyhow::Result<(FileId, VfsPath)> {
        let db = self.host.raw_database();
        let file_id = krate.data(db).root_file_id;
        let path = path_for_file(db, file_id)?;

        Ok((file_id, VfsPath::new_real_path(path)))
    }

    pub fn next_session_file_id(&self) -> FileId {
        let next = self
            .loaded_workspaces
            .iter()
            .flat_map(|workspace| workspace._vfs.iter().map(|(file_id, _)| file_id.index()))
            .max()
            .map_or(0, |index| index + 1);

        FileId::from_raw(next)
    }

    pub fn package_instances(&self) -> impl Iterator<Item = &PackageInstance> {
        self.package_instances.values()
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

        for krate in all_crates(db).iter().copied() {
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
}

impl WorkspaceView {
    pub fn new(workspaces: Vec<usize>) -> Self {
        Self { workspaces }
    }

    pub fn push_workspace(&mut self, workspace: usize) {
        self.workspaces.push(workspace);
    }

    pub fn workspace_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.workspaces.iter().copied()
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

        overlay.materialize_files(world.next_session_file_id());

        Ok(overlay)
    }
}

fn load_cargo_workspace_into_host(
    host: &mut AnalysisHost,
    root: impl AsRef<Path>,
    config: &SharedAnalyzerConfig,
) -> anyhow::Result<LoadedWorkspace> {
    let root = AbsPathBuf::assert_utf8(std::fs::canonicalize(root)?);
    let manifest = ProjectManifest::discover_single(&root)?;
    let manifest_path = manifest.manifest_path().clone();
    let workspace = ProjectWorkspace::load(manifest, &config.cargo_config, &|_| {})?;
    let root = workspace.workspace_root().to_string();
    let packages = workspace.n_packages();
    let workspace_for_session = workspace.clone();
    let (vfs, proc_macro_client) = {
        let db = host.raw_database_mut();
        load_workspace_into_db(
            workspace,
            &config.cargo_config.extra_env,
            &config.load.to_load_cargo_config(),
            db,
        )?
    };
    let files = vfs.iter().count();
    let line_endings = {
        let db = host.raw_database();
        vfs.iter()
            .map(|(file_id, _)| {
                let text = db.file_text(file_id).text(db).to_string();
                let (_, line_endings) = crate::line_index::LineEndings::normalize(text);
                (file_id, line_endings)
            })
            .collect()
    };

    Ok(LoadedWorkspace {
        summary: WorkspaceSummary {
            root,
            manifest: manifest_path.to_string(),
            packages,
            files,
            proc_macro_server: proc_macro_client.is_some(),
        },
        workspace: workspace_for_session,
        _vfs: vfs,
        line_endings,
        _proc_macro_client: proc_macro_client,
    })
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
    use std::{env, sync::Once};

    use crossbeam_channel::{Receiver, Sender};
    use ide::FileId;
    use lsp_server::{Connection, Message};
    use lsp_types::Url;
    use paths::Utf8PathBuf;
    use triomphe::Arc;
    use vfs::AbsPathBuf;

    use crate::{
        analyzed_bridge::{
            SharedAnalyzerSession, SharedAnalyzerSnapshot, patch_path_prefix,
        },
        config::{Config, ConfigChange, ConfigErrors},
        from_json, server_capabilities, version,
        global_state::{FetchWorkspaceRequest, FetchWorkspaceResponse},
        line_index::LineEndings,
    };

    pub(crate) struct RustAnalyzerSession {
        state: crate::global_state::GlobalState,
    }

    impl RustAnalyzerSession {
        pub(crate) fn new(sender: Sender<Message>, config: crate::config::Config) -> Self {
            Self {
                state: crate::global_state::GlobalState::new(sender, config),
            }
        }

        pub(crate) fn new_with_shared_snapshot(
            sender: Sender<Message>,
            config: crate::config::Config,
            snapshot: SharedAnalyzerSnapshot,
        ) -> anyhow::Result<Self> {
            let mut session = Self::new(sender, config);
            let workspaces = snapshot.workspaces;
            session.state.analyzed_shared = Some(snapshot.runtime);
            install_shared_workspaces(&mut session.state, workspaces);

            Ok(session)
        }

        pub(crate) fn run_shared(self, receiver: Receiver<Message>) -> anyhow::Result<()> {
            run_shared_state(self.state, receiver)
        }
    }

    pub(crate) fn run_shared_lsp_session(
        connection: Connection,
        session: SharedAnalyzerSession,
    ) -> anyhow::Result<()> {
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
        let snapshot = session.snapshot()?;
        let Connection { sender, receiver } = connection;
        RustAnalyzerSession::new_with_shared_snapshot(sender, config, snapshot)?.run_shared(receiver)
    }

    fn install_shared_workspaces(
        state: &mut crate::global_state::GlobalState,
        workspaces: Vec<project_model::ProjectWorkspace>,
    ) {
        let fetched_workspaces = workspaces.iter().cloned().map(Ok).collect();
        let source_root_config = {
            let files_config = state.config.files();
            load_cargo::ProjectFolders::new(
                &workspaces,
                &files_config.exclude,
                Config::user_config_dir_path().as_deref(),
            )
            .source_root_config
        };
        state.source_root_config = source_root_config;
        state.local_roots_parent_map = Arc::new(state.source_root_config.source_root_parent_map());
        state.workspaces = Arc::new(workspaces);
        state.fetch_workspaces_queue.request_op(
            "startup".to_owned(),
            FetchWorkspaceRequest {
                path: None,
                force_crate_graph_reload: false,
            },
        );
        _ = state.fetch_workspaces_queue.should_start_op();
        state.fetch_workspaces_queue.op_completed(FetchWorkspaceResponse {
            workspaces: fetched_workspaces,
            force_crate_graph_reload: false,
        });
        state.vfs_done = true;
        state.finish_loading_crate_graph();

        if state.config.check_on_save(None)
            && state.config.flycheck_workspace(None)
            && !state.fetch_build_data_queue.op_requested()
        {
            state
                .flycheck
                .iter()
                .for_each(|flycheck| flycheck.restart_workspace(None));
        }
    }

    fn run_shared_state(
        mut state: crate::global_state::GlobalState,
        inbox: Receiver<Message>,
    ) -> anyhow::Result<()> {
        state.update_status_or_notify();

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
            state.handle_event(event);
        }

        anyhow::bail!("A receiver has been dropped, something panicked!")
    }

    impl crate::global_state::GlobalState {
        pub(crate) fn analyzed_process_shared_changes(&mut self) -> bool {
            let Some(shared) = self.analyzed_shared.clone() else {
                return false;
            };

            let mut guard = self.vfs.write();
            let changed_files = guard.0.take_changes();
            if changed_files.is_empty() {
                return false;
            }

            let mut modified_rust_files = Vec::new();
            let mut changed = false;
            for file in changed_files.into_values() {
                let vfs_path = guard.0.file_path(file.file_id).clone();
                let is_modified = file.is_modified();
                let exists = file.exists();
                let is_rust_file = vfs_path
                    .as_path()
                    .is_some_and(|path| path.extension() == Some("rs"));
                let text = match file.change {
                    vfs::Change::Create(contents, _) | vfs::Change::Modify(contents, _) => {
                        String::from_utf8(contents).ok().map(LineEndings::normalize)
                    }
                    vfs::Change::Delete => None,
                };

                match shared.apply_file_change(&vfs_path, text) {
                    Ok(Some(file_id)) => {
                        changed = true;
                        if !exists {
                            self.diagnostics.clear_native_for(file_id);
                        }
                        if is_modified && is_rust_file {
                            modified_rust_files.push(file_id);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!("failed to apply shared analyzer change for {vfs_path}: {error}");
                    }
                }
            }
            drop(guard);

            if !modified_rust_files.is_empty() {
                _ = self
                    .deferred_task_queue
                    .sender
                    .send(crate::main_loop::DeferredTask::CheckProcMacroSources(modified_rust_files));
            }

            changed
        }

        pub(crate) fn analyzed_file_id_to_url(&self, file_id: FileId) -> Url {
            if let Some(shared) = &self.analyzed_shared
                && let Some(url) = shared.file_id_to_url(file_id)
            {
                return url;
            }

            crate::global_state::file_id_to_url(&self.vfs.read().0, file_id)
        }

        pub(crate) fn analyzed_url_to_file_id(
            &self,
            url: &Url,
        ) -> anyhow::Result<Option<FileId>> {
            if let Some(shared) = &self.analyzed_shared {
                return shared.url_to_file_id(url);
            }

            crate::global_state::url_to_file_id(&self.vfs.read().0, url)
        }
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
