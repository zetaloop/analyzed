use std::{
    env,
    error::Error,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use toml::{Table, Value, map::Map};

mod edit;

pub use edit::*;
pub use ra_ap_syntax::ast;

#[derive(Debug)]
pub struct LockedPackage {
    pub version: String,
    pub checksum: String,
    pub git_revision: Option<String>,
}

pub fn prepare_bridge_package(
    package_name: &str,
    generated_dir: &str,
) -> Result<(PathBuf, LockedPackage), Box<dyn Error>> {
    let manifest = bridge_manifest_path();
    let (mut package, lock) = locked_package(package_name, &manifest)?;
    let archive = registry_archive(package_name, &package, &manifest)?;
    verify_archive_checksum(&archive, &package)?;

    let generated =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo")).join(generated_dir);
    unpack_crate_archive(package_name, &archive, &generated, &package)?;
    verify_manifest_matches_bridge(package_name, &generated.join("Cargo.toml"), &manifest)?;
    package.git_revision = crate_git_revision(&generated)?;
    rewrite_lib_header(&generated.join("src/lib.rs"))?;

    println!("cargo:rerun-if-changed={}", archive.display());
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", lock.display());

    Ok((generated, package))
}

fn registry_archive(
    package_name: &str,
    package: &LockedPackage,
    manifest: &Path,
) -> Result<PathBuf, Box<dyn Error>> {
    let cargo_home = cargo_home()?;
    if let Some(archive) = find_registry_archive(package_name, &cargo_home, package)? {
        return Ok(archive);
    }

    fetch_registry_packages(manifest)?;

    if let Some(archive) = find_registry_archive(package_name, &cargo_home, package)? {
        return Ok(archive);
    }

    Err(format!(
        "could not find {} under {} after `cargo fetch --locked`",
        archive_name(package_name, package),
        cargo_home.join("registry").join("cache").display()
    )
    .into())
}

fn cargo_home() -> Result<PathBuf, Box<dyn Error>> {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .or_else(|| env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or_else(|| "CARGO_HOME is unavailable".into())
}

fn find_registry_archive(
    package_name: &str,
    cargo_home: &Path,
    package: &LockedPackage,
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let registry_cache = cargo_home.join("registry").join("cache");
    let package_archive = archive_name(package_name, package);
    let Ok(registries) = fs::read_dir(&registry_cache) else {
        return Ok(None);
    };

    for registry in registries {
        let candidate = registry?.path().join(&package_archive);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn fetch_registry_packages(manifest: &Path) -> Result<(), Box<dyn Error>> {
    let cargo = env::var_os("CARGO").ok_or("CARGO is unavailable")?;
    let status = Command::new(cargo)
        .arg("fetch")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(manifest)
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

fn locked_package(
    package_name: &str,
    manifest: &Path,
) -> Result<(LockedPackage, PathBuf), Box<dyn Error>> {
    let version = upstream_package_version(package_name, manifest)?;
    let locks = lockfile_candidates(manifest);

    for lock in locks.iter().filter(|lock| lock.is_file()) {
        if let Some(checksum) = locked_package_checksum(package_name, lock, &version)? {
            return Ok((
                LockedPackage {
                    version: version.clone(),
                    checksum,
                    git_revision: None,
                },
                lock.clone(),
            ));
        }
    }

    Err(format!(
        "could not find registry {package_name} {version} checksum in {}",
        locks
            .iter()
            .map(|lock| lock.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
    .into())
}

fn bridge_manifest_path() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("Cargo.toml")
}

fn upstream_package_version(package_name: &str, manifest: &Path) -> Result<String, Box<dyn Error>> {
    let manifest = read_manifest(manifest)?;
    let dependencies = manifest_table(&manifest, &["target", "cfg(any())", "dependencies"])
        .ok_or("bridge manifest has no cfg(any()) source dependency")?;
    let version = dependencies
        .iter()
        .find(|(name, dependency)| dependency_package_name(name, dependency) == package_name)
        .and_then(|(_, dependency)| dependency_version(dependency))
        .ok_or_else(|| {
            format!("bridge manifest has no cfg(any()) dependency for {package_name}")
        })?;

    exact_version(version)
}

fn dependency_package_name<'a>(name: &'a str, dependency: &'a Value) -> &'a str {
    dependency
        .as_table()
        .and_then(|dependency| dependency.get("package"))
        .and_then(Value::as_str)
        .unwrap_or(name)
}

fn dependency_version(dependency: &Value) -> Option<&str> {
    match dependency {
        Value::String(version) => Some(version),
        Value::Table(dependency) => dependency.get("version").and_then(Value::as_str),
        _ => None,
    }
}

fn exact_version(version: &str) -> Result<String, Box<dyn Error>> {
    version
        .strip_prefix('=')
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!("bridge source dependency version must be exact, got {version}").into()
        })
}

fn lockfile_candidates(manifest: &Path) -> Vec<PathBuf> {
    let manifest_dir = manifest
        .parent()
        .expect("manifest path has a parent directory");
    let package_lock = manifest_dir.join("Cargo.lock");
    let workspace_lock = manifest_dir.join("../..").join("Cargo.lock");
    if package_lock == workspace_lock {
        vec![package_lock]
    } else {
        vec![package_lock, workspace_lock]
    }
}

fn crate_git_revision(generated: &Path) -> Result<Option<String>, Box<dyn Error>> {
    let path = generated.join(".cargo_vcs_info.json");
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let info: serde_json::Value = serde_json::from_str(&source)?;
    let revision = info
        .get("git")
        .and_then(|git| git.get("sha1"))
        .and_then(serde_json::Value::as_str)
        .ok_or(".cargo_vcs_info.json does not contain git.sha1")?;
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid git.sha1 in .cargo_vcs_info.json: {revision}").into());
    }
    Ok(Some(revision.to_ascii_lowercase()))
}

fn locked_package_checksum(
    package_name: &str,
    lock: &Path,
    target_version: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let lock = fs::read_to_string(lock)?;
    let mut name = None;
    let mut version = None;
    let mut source = None;
    let mut checksum = None;

    for line in lock.lines().chain(["[[package]]"]) {
        if line == "[[package]]" {
            if name.as_deref() == Some(package_name)
                && version.as_deref() == Some(target_version)
                && source
                    .as_deref()
                    .is_some_and(|source: &str| source.starts_with("registry+"))
            {
                return Ok(checksum);
            }
            name = None;
            version = None;
            source = None;
            checksum = None;
            continue;
        }

        if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("version = ") {
            version = Some(value.trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("source = ") {
            source = Some(value.trim_matches('"').to_owned());
        } else if let Some(value) = line.strip_prefix("checksum = ") {
            checksum = Some(value.trim_matches('"').to_owned());
        }
    }

    Ok(None)
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unpack_crate_archive(
    package_name: &str,
    archive: &Path,
    destination: &Path,
    package: &LockedPackage,
) -> Result<(), Box<dyn Error>> {
    let decoder = GzDecoder::new(fs::File::open(archive)?);
    let mut archive = tar::Archive::new(decoder);
    let package_dir = package_dir(package_name, package);

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

fn verify_manifest_matches_bridge(
    package_name: &str,
    upstream_manifest_path: &Path,
    bridge_manifest_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let upstream_manifest = read_manifest(upstream_manifest_path)?;
    let bridge_manifest = read_manifest(bridge_manifest_path)?;
    let mut mismatches = Vec::new();

    compare_manifest_section(
        "dependencies",
        normalized_dependencies(&upstream_manifest, None),
        normalized_dependencies(
            &bridge_manifest,
            manifest_table(&upstream_manifest, &["dependencies"]),
        ),
        true,
        &mut mismatches,
    );
    compare_manifest_section(
        "target",
        normalized_target_dependencies(&upstream_manifest, None),
        normalized_target_dependencies(&bridge_manifest, Some(&upstream_manifest)),
        true,
        &mut mismatches,
    );
    compare_manifest_section(
        "features",
        manifest_section(&upstream_manifest, &["features"]),
        manifest_section(&bridge_manifest, &["features"]),
        false,
        &mut mismatches,
    );
    compare_manifest_section(
        "lints",
        manifest_section(&upstream_manifest, &["lints"]),
        manifest_section(&bridge_manifest, &["lints"]),
        false,
        &mut mismatches,
    );

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "bridge Cargo.toml is out of sync with {package_name}:\n{}",
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
    allow_extra: bool,
    mismatches: &mut Vec<String>,
) {
    match (expected, actual) {
        (Some(expected), Some(actual)) if expected == actual => {}
        (Some(Value::Table(expected)), Some(Value::Table(actual))) => {
            compare_manifest_tables(label, &expected, &actual, allow_extra, mismatches);
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
    allow_extra: bool,
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
    if !actual_only.is_empty() && !allow_extra {
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

fn normalized_dependencies(manifest: &Value, reference: Option<&Table>) -> Option<Value> {
    Some(Value::Table(normalize_dependencies(
        manifest_table(manifest, &["dependencies"])?,
        reference,
    )))
}

fn normalized_target_dependencies(manifest: &Value, reference: Option<&Value>) -> Option<Value> {
    let targets = manifest_table(manifest, &["target"])?;
    let mut normalized_targets = Map::new();

    for (target, target_value) in targets {
        if target == "cfg(any())" {
            continue;
        }
        let Some(target_table) = target_value.as_table() else {
            continue;
        };
        let Some(dependencies) = target_table.get("dependencies").and_then(Value::as_table) else {
            continue;
        };
        let reference_dependencies = reference.and_then(|reference| {
            manifest_table(reference, &["target", target.as_str(), "dependencies"])
        });
        let mut normalized_target = Map::new();
        normalized_target.insert(
            "dependencies".to_owned(),
            Value::Table(normalize_dependencies(dependencies, reference_dependencies)),
        );
        normalized_targets.insert(target.clone(), Value::Table(normalized_target));
    }

    if normalized_targets.is_empty() {
        None
    } else {
        Some(Value::Table(normalized_targets))
    }
}

fn normalize_dependencies(dependencies: &Table, reference: Option<&Table>) -> Table {
    dependencies
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                normalize_dependency(value, reference.and_then(|reference| reference.get(name))),
            )
        })
        .collect()
}

fn normalize_dependency(value: &Value, reference: Option<&Value>) -> Value {
    let Some(mut dependency) = dependency_table(value) else {
        return value.clone();
    };
    let Some(reference) = reference.and_then(dependency_table) else {
        return Value::Table(dependency);
    };

    let had_path = dependency.remove("path").is_some();
    if had_path || is_replaced_package_dependency(&dependency, &reference) {
        copy_dependency_field(&mut dependency, &reference, "package");
        copy_dependency_field(&mut dependency, &reference, "version");
    }

    Value::Table(dependency)
}

fn dependency_table(value: &Value) -> Option<Table> {
    match value {
        Value::String(version) => {
            let mut dependency = Map::new();
            dependency.insert("version".to_owned(), Value::String(version.clone()));
            Some(dependency)
        }
        Value::Table(dependency) => Some(dependency.clone()),
        _ => None,
    }
}

fn is_replaced_package_dependency(dependency: &Table, reference: &Table) -> bool {
    dependency.get("package").is_some()
        && dependency.get("package") != reference.get("package")
        && dependency.get("version") != reference.get("version")
}

fn copy_dependency_field(dependency: &mut Table, reference: &Table, field: &str) {
    if let Some(value) = reference.get(field) {
        dependency.insert(field.to_owned(), value.clone());
    } else {
        dependency.remove(field);
    }
}

fn table_keys(table: &Table) -> Vec<String> {
    let mut keys = table.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn package_dir(package_name: &str, package: &LockedPackage) -> String {
    format!("{package_name}-{}", package.version)
}

fn archive_name(package_name: &str, package: &LockedPackage) -> String {
    format!("{}.crate", package_dir(package_name, package))
}
