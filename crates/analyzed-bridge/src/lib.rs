use std::{
    env,
    error::Error,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use flate2::read::GzDecoder;
use ra_ap_syntax::{
    AstNode, Edition, SourceFile, SyntaxNode,
    ast::{self, HasLoopBody, HasName, HasVisibility},
};
use sha2::{Digest, Sha256};
use toml::{Table, Value, map::Map};

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

pub fn inject_use(source: &mut String, path: &str) -> Result<(), Box<dyn Error>> {
    let statement = format!("use {path};\n");
    if source.contains(&statement) {
        return Err(format!("source already contains `{}`", statement.trim_end()).into());
    }

    let index =
        first_use_index(source).unwrap_or_else(|| insertion_index_after_inner_attrs(source));
    source.insert_str(index, &statement);
    Ok(())
}

pub fn inject_pub_use(source: &mut String, path: &str) -> Result<(), Box<dyn Error>> {
    let statement = format!("pub(crate) use {path};\n");
    if source.contains(&statement) {
        return Err(format!("source already contains `{}`", statement.trim_end()).into());
    }

    let index =
        first_use_index(source).unwrap_or_else(|| insertion_index_after_inner_attrs(source));
    source.insert_str(index, &statement);
    Ok(())
}

pub fn rename_function(
    source: &mut String,
    name: &str,
    replacement: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, name)?;
    let name = function.name().ok_or("function has no name")?;
    replace_text_range(source, name.syntax().text_range(), replacement);
    Ok(())
}

pub fn allow_dead_code_for_function(source: &mut String, name: &str) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, name)?;
    let start = text_offset(function.syntax().text_range().start());
    let indent = line_indent(source, start);
    source.insert_str(start, &format!("{indent}#[allow(dead_code)]\n"));
    Ok(())
}

pub fn widen_function_visibility(
    source: &mut String,
    name: &str,
    visibility: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, name)?;
    if let Some(existing) = function.visibility() {
        replace_text_range(source, existing.syntax().text_range(), visibility);
    } else {
        let token = function.fn_token().ok_or("function has no fn token")?;
        let start = text_offset(token.text_range().start());
        source.insert_str(start, &format!("{visibility} "));
    }
    Ok(())
}

pub fn widen_enum_visibility(
    source: &mut String,
    name: &str,
    visibility: &str,
) -> Result<(), Box<dyn Error>> {
    let enumeration = parse_source(source)?
        .descendants()
        .filter_map(ast::Enum::cast)
        .find(|item| item.name().is_some_and(|it| it.text() == name))
        .ok_or_else(|| format!("could not find enum `{name}`"))?;
    if let Some(vis) = enumeration.visibility() {
        replace_text_range(source, vis.syntax().text_range(), visibility);
    } else {
        let start = text_offset(enumeration.syntax().text_range().start());
        source.insert_str(start, &format!("{visibility} "));
    }
    Ok(())
}

pub fn rename_path_root_in_function(
    source: &mut String,
    function: &str,
    root: &str,
    replacement: &str,
) -> Result<usize, Box<dyn Error>> {
    let function = find_function(source, function)?;
    let mut ranges = Vec::new();
    for node in function.syntax().descendants() {
        let Some(segment) = ast::PathSegment::cast(node) else {
            continue;
        };
        let Some(name) = segment.name_ref() else {
            continue;
        };
        if name.text() != root {
            continue;
        }
        let Some(path) = segment.syntax().parent().and_then(ast::Path::cast) else {
            continue;
        };
        if path.qualifier().is_some() {
            continue;
        }
        ranges.push(name.syntax().text_range());
    }
    let count = ranges.len();
    for range in ranges.into_iter().rev() {
        replace_text_range(source, range, replacement);
    }
    Ok(count)
}

pub struct MethodParam<'a> {
    pub name: &'a str,
    pub ty: &'a str,
}

pub struct ExtractedMethod<'a> {
    pub name: &'a str,
    pub receiver: Option<&'a str>,
    pub params: &'a [MethodParam<'a>],
    pub args: &'a [&'a str],
}

#[derive(Clone, Copy)]
pub enum ExtractSelector<'a> {
    LetBinding(&'a str),
    ForLoopBinding(&'a str),
    TopLevelMethodCall(&'a str),
}

pub enum ExtractRange<'a> {
    TailToBlockEnd,
    Body,
    StatementSequence { len: usize },
    Initializer { return_ty: &'a str },
}

pub fn extract_method(
    source: &mut String,
    function: &str,
    selector: ExtractSelector<'_>,
    occurrence: usize,
    range: ExtractRange<'_>,
    method: ExtractedMethod<'_>,
) -> Result<(), Box<dyn Error>> {
    let function_node = find_function(source, function)?;
    let function_end = text_offset(function_node.syntax().text_range().end());
    let function_indent = line_indent(
        source,
        text_offset(function_node.syntax().text_range().start()),
    )
    .to_owned();
    let extraction = match range {
        ExtractRange::TailToBlockEnd => {
            let ExtractSelector::LetBinding(_) = selector else {
                return Err("TailToBlockEnd requires a let binding selector".into());
            };
            let body = function_node
                .body()
                .ok_or_else(|| format!("function `{function}` has no body"))?;
            let stmt_list = body
                .stmt_list()
                .ok_or_else(|| format!("function `{function}` has no statement list"))?;
            let anchor = find_statement_in_list(&stmt_list, &selector, occurrence)?;
            let first_extracted_stmt = stmt_list
                .statements()
                .find(|statement| {
                    statement.syntax().text_range().start() > anchor.text_range().end()
                })
                .ok_or_else(|| format!("function `{function}` has no statements after anchor"))?;
            let closing_brace = stmt_list
                .r_curly_token()
                .ok_or_else(|| format!("function `{function}` has no closing brace"))?;
            let range = text_offset(anchor.text_range().end())
                ..text_offset(closing_brace.text_range().start());
            Extraction {
                call_indent: line_indent(
                    source,
                    text_offset(first_extracted_stmt.syntax().text_range().start()),
                )
                .to_owned(),
                close_indent: function_indent.clone(),
                body: source[range.clone()].trim_end().to_owned(),
                return_ty: None,
                expression: false,
                range,
            }
        }
        ExtractRange::Body => {
            let ExtractSelector::ForLoopBinding(binding) = selector else {
                return Err("Body requires a for loop selector".into());
            };
            let loop_expr = find_for_expr_by_binding(&function_node, binding, occurrence)?;
            let body = loop_expr
                .loop_body()
                .ok_or_else(|| format!("for `{binding}` has no body"))?;
            let stmt_list = body
                .stmt_list()
                .ok_or_else(|| format!("for `{binding}` has no statement list"))?;
            let first_statement = stmt_list
                .statements()
                .next()
                .ok_or_else(|| format!("for `{binding}` has empty body"))?;
            let opening_brace = stmt_list
                .l_curly_token()
                .ok_or_else(|| format!("for `{binding}` has no opening brace"))?;
            let closing_brace = stmt_list
                .r_curly_token()
                .ok_or_else(|| format!("for `{binding}` has no closing brace"))?;
            let range = text_offset(opening_brace.text_range().end())
                ..text_offset(closing_brace.text_range().start());
            Extraction {
                call_indent: line_indent(
                    source,
                    text_offset(first_statement.syntax().text_range().start()),
                )
                .to_owned(),
                close_indent: line_indent(source, text_offset(closing_brace.text_range().start()))
                    .to_owned(),
                body: source[range.clone()].trim_end().to_owned(),
                return_ty: None,
                expression: false,
                range,
            }
        }
        ExtractRange::StatementSequence { len } => {
            let (stmt_list, statements, index) =
                find_statement_sequence_start(&function_node, &selector, occurrence)?;
            let includes_tail_expr = index + len == statements.len() + 1;
            if index + len > statements.len() && !includes_tail_expr {
                return Err("statement sequence exceeds statement list".into());
            }
            let first_statement = &statements[index];
            let tail_expr = includes_tail_expr.then(|| stmt_list.tail_expr()).flatten();
            let end_range = tail_expr
                .as_ref()
                .map(|expr| expr.syntax().text_range())
                .unwrap_or_else(|| statements[index + len - 1].syntax().text_range());
            let first_start = text_offset(first_statement.syntax().text_range().start());
            let range = line_start_offset(source, first_start)..text_offset(end_range.end());
            let call_indent = line_indent(source, first_start).to_owned();
            let method_body_indent = format!("{function_indent}    ");
            Extraction {
                call_indent: call_indent.clone(),
                close_indent: String::new(),
                body: normalize_statement_body(
                    &source[range.clone()],
                    &call_indent,
                    &method_body_indent,
                ),
                return_ty: None,
                expression: false,
                range,
            }
        }
        ExtractRange::Initializer { return_ty } => {
            let ExtractSelector::LetBinding(_) = selector else {
                return Err("Initializer requires a let binding selector".into());
            };
            let statement = function_node
                .syntax()
                .descendants()
                .filter_map(ast::LetStmt::cast)
                .filter(|statement| {
                    let_statement_has_ident_binding(statement, selector_name(selector))
                })
                .nth(occurrence)
                .ok_or_else(|| format!("function `{function}` has no selected let binding"))?;
            let initializer = statement
                .initializer()
                .ok_or_else(|| "selected let binding has no initializer".to_owned())?;
            let method_body_indent = format!("{function_indent}    ");
            Extraction {
                call_indent: String::new(),
                close_indent: String::new(),
                body: format!("\n{method_body_indent}{}", initializer.syntax().text()),
                return_ty: Some(return_ty.to_owned()),
                expression: true,
                range: text_range(initializer.syntax().text_range()),
            }
        }
    };
    apply_extraction(source, function_end, &function_indent, extraction, method)
}

pub fn extract_method_from_unique_for_loop(
    source: &mut String,
    function: &str,
    return_ty: &str,
    method: ExtractedMethod<'_>,
) -> Result<(), Box<dyn Error>> {
    let function_node = find_function(source, function)?;
    let function_end = text_offset(function_node.syntax().text_range().end());
    let function_indent = line_indent(
        source,
        text_offset(function_node.syntax().text_range().start()),
    )
    .to_owned();
    let mut for_loops = function_node
        .syntax()
        .descendants()
        .filter_map(ast::ForExpr::cast);
    let loop_expr = for_loops
        .next()
        .ok_or_else(|| format!("function `{function}` has no for loop"))?;
    if for_loops.next().is_some() {
        return Err(format!("function `{function}` has more than one for loop").into());
    }
    let body = function_node
        .body()
        .ok_or_else(|| format!("function `{function}` has no body"))?;
    let stmt_list = body
        .stmt_list()
        .ok_or_else(|| format!("function `{function}` has no statement list"))?;
    let tail = stmt_list
        .tail_expr()
        .ok_or_else(|| format!("function `{function}` has no tail expression"))?;
    let start = text_offset(loop_expr.syntax().text_range().start());
    let end = text_offset(tail.syntax().text_range().end());
    if end <= start {
        return Err(format!("function `{function}` tail expression precedes its for loop").into());
    }
    let call_indent = line_indent(source, start).to_owned();
    let extraction = Extraction {
        body: format!("\n{call_indent}{}", &source[start..end]),
        range: start..end,
        call_indent,
        close_indent: String::new(),
        return_ty: Some(return_ty.to_owned()),
        expression: true,
    };
    apply_extraction(source, function_end, &function_indent, extraction, method)
}

fn apply_extraction(
    source: &mut String,
    function_end: usize,
    function_indent: &str,
    extraction: Extraction,
    method: ExtractedMethod<'_>,
) -> Result<(), Box<dyn Error>> {
    let params = method
        .params
        .iter()
        .map(|param| format!("{}: {}", param.name, param.ty))
        .collect::<Vec<_>>()
        .join(", ");
    let params = match (method.receiver, params.is_empty()) {
        (Some(receiver), true) => receiver.to_owned(),
        (Some(receiver), false) => format!("{receiver}, {params}"),
        (None, _) => params,
    };
    let args = method.args.join(", ");
    let replacement = if extraction.expression {
        format!("self.{}({args})", method.name)
    } else {
        format!(
            "\n{}self.{}({args});\n{}",
            extraction.call_indent, method.name, extraction.close_indent,
        )
    };

    let return_ty = extraction
        .return_ty
        .as_deref()
        .map_or(String::new(), |ty| format!(" -> {ty}"));
    let extracted = format!(
        "\n\n{function_indent}fn {}({params}){return_ty} {{{}\n{function_indent}}}",
        method.name, extraction.body,
    );
    source.insert_str(function_end, &extracted);
    source.replace_range(extraction.range, &replacement);
    parse_source(source)?;
    Ok(())
}

struct Extraction {
    range: std::ops::Range<usize>,
    body: String,
    call_indent: String,
    close_indent: String,
    return_ty: Option<String>,
    expression: bool,
}

fn selector_name<'a>(selector: ExtractSelector<'a>) -> &'a str {
    match selector {
        ExtractSelector::LetBinding(name)
        | ExtractSelector::ForLoopBinding(name)
        | ExtractSelector::TopLevelMethodCall(name) => name,
    }
}

fn let_statement_has_ident_binding(statement: &ast::LetStmt, name: &str) -> bool {
    pat_is_ident(statement.pat(), name)
}

fn find_for_expr_by_binding(
    function: &ast::Fn,
    binding: &str,
    occurrence: usize,
) -> Result<ast::ForExpr, Box<dyn Error>> {
    function
        .syntax()
        .descendants()
        .filter_map(ast::ForExpr::cast)
        .filter(|expr| pat_is_ident(expr.pat(), binding))
        .nth(occurrence)
        .ok_or_else(|| {
            format!("function has no for loop binding `{binding}` occurrence {occurrence}").into()
        })
}

fn find_statement_in_list(
    stmt_list: &ast::StmtList,
    selector: &ExtractSelector<'_>,
    occurrence: usize,
) -> Result<SyntaxNode, Box<dyn Error>> {
    stmt_list
        .statements()
        .filter(|statement| statement_matches_selector(statement, selector))
        .nth(occurrence)
        .map(|statement| statement.syntax().clone())
        .ok_or_else(|| format!("could not find statement occurrence {occurrence}").into())
}

fn find_statement_sequence_start(
    function: &ast::Fn,
    selector: &ExtractSelector<'_>,
    occurrence: usize,
) -> Result<(ast::StmtList, Vec<ast::Stmt>, usize), Box<dyn Error>> {
    let mut matches = Vec::new();
    for stmt_list in function
        .syntax()
        .descendants()
        .filter_map(ast::StmtList::cast)
    {
        let statements = stmt_list.statements().collect::<Vec<_>>();
        for (index, statement) in statements.iter().enumerate() {
            if statement_matches_selector(statement, selector) {
                matches.push((stmt_list.clone(), statements.clone(), index));
            }
        }
    }
    matches
        .into_iter()
        .nth(occurrence)
        .ok_or_else(|| format!("could not find statement occurrence {occurrence}").into())
}

fn statement_matches_selector(statement: &ast::Stmt, selector: &ExtractSelector<'_>) -> bool {
    match selector {
        ExtractSelector::LetBinding(name) => match statement {
            ast::Stmt::LetStmt(let_statement) => {
                let_statement_has_ident_binding(let_statement, name)
            }
            _ => false,
        },
        ExtractSelector::TopLevelMethodCall(method) => match statement {
            ast::Stmt::ExprStmt(statement) => statement
                .expr()
                .and_then(|expr| match expr {
                    ast::Expr::MethodCallExpr(call) => Some(call),
                    _ => None,
                })
                .is_some_and(|call| call.name_ref().is_some_and(|name| name.text() == *method)),
            _ => false,
        },
        ExtractSelector::ForLoopBinding(_) => false,
    }
}

fn line_start_offset(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn normalize_statement_body(body: &str, source_indent: &str, target_indent: &str) -> String {
    let mut normalized = String::new();
    for line in body.trim_end().lines() {
        normalized.push('\n');
        normalized.push_str(target_indent);
        normalized.push_str(line.strip_prefix(source_indent).unwrap_or(line));
    }
    normalized
}

fn pat_is_ident(pattern: Option<ast::Pat>, name: &str) -> bool {
    matches!(pattern, Some(ast::Pat::IdentPat(pattern)) if pattern.name().is_some_and(|it| it.text() == name))
}

pub fn add_record_pattern_rest(
    source: &mut String,
    function: &str,
    path_tail: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, function)?;
    for node in function.syntax().descendants() {
        let Some(record) = ast::RecordPat::cast(node) else {
            continue;
        };
        let Some(path) = record.path() else {
            continue;
        };
        if !path.syntax().text().to_string().ends_with(path_tail) {
            continue;
        }
        let Some(fields) = record.record_pat_field_list() else {
            continue;
        };
        if fields.rest_pat().is_some() {
            return Ok(());
        }
        let token = fields
            .r_curly_token()
            .ok_or("record pattern has no closing brace")?;
        let start = text_offset(token.text_range().start());
        source.insert_str(start, ", ..");
        return Ok(());
    }
    Err(format!(
        "function `{}` has no `{}` record pattern",
        function
            .name()
            .map(|it| it.text().to_string())
            .unwrap_or_default(),
        path_tail
    )
    .into())
}

pub fn append_struct_fields(
    source: &mut String,
    struct_name: &str,
    fields: &str,
) -> Result<(), Box<dyn Error>> {
    let structure = parse_source(source)?
        .descendants()
        .filter_map(ast::Struct::cast)
        .find(|item| item.name().is_some_and(|it| it.text() == struct_name))
        .ok_or_else(|| format!("could not find struct `{struct_name}`"))?;
    let Some(ast::FieldList::RecordFieldList(fields_list)) = structure.field_list() else {
        return Err(format!("struct `{struct_name}` has no record field list").into());
    };
    let token = fields_list
        .r_curly_token()
        .ok_or("record struct has no closing brace")?;
    let start = text_offset(token.text_range().start());
    source.insert_str(start, fields);
    Ok(())
}

pub fn add_struct_attribute(
    source: &mut String,
    struct_name: &str,
    attribute: &str,
) -> Result<(), Box<dyn Error>> {
    let structure = find_struct(source, struct_name)?;
    let start = text_offset(structure.syntax().text_range().start());
    let indent = line_indent(source, start);
    source.insert_str(start, &format!("{indent}{attribute}\n"));
    Ok(())
}

pub fn add_function_attribute(
    source: &mut String,
    function: &str,
    attribute: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, function)?;
    let start = text_offset(function.syntax().text_range().start());
    let indent = line_indent(source, start);
    source.insert_str(start, &format!("{indent}{attribute}\n"));
    Ok(())
}

pub fn widen_struct_field_visibility(
    source: &mut String,
    struct_name: &str,
    field_name: &str,
    visibility: &str,
) -> Result<(), Box<dyn Error>> {
    let structure = find_struct(source, struct_name)?;
    let Some(ast::FieldList::RecordFieldList(fields_list)) = structure.field_list() else {
        return Err(format!("struct `{struct_name}` has no record field list").into());
    };
    for field in fields_list.fields() {
        let Some(name) = field.name() else {
            continue;
        };
        if name.text() != field_name {
            continue;
        }
        if let Some(existing) = field.visibility() {
            replace_text_range(source, existing.syntax().text_range(), visibility);
        } else {
            let start = text_offset(name.syntax().text_range().start());
            source.insert_str(start, &format!("{visibility} "));
        }
        return Ok(());
    }
    Err(format!("struct `{struct_name}` has no `{field_name}` field").into())
}

pub fn add_struct_field_attribute(
    source: &mut String,
    struct_name: &str,
    field_name: &str,
    attribute: &str,
) -> Result<(), Box<dyn Error>> {
    let structure = find_struct(source, struct_name)?;
    let Some(ast::FieldList::RecordFieldList(fields_list)) = structure.field_list() else {
        return Err(format!("struct `{struct_name}` has no record field list").into());
    };
    for field in fields_list.fields() {
        let Some(name) = field.name() else {
            continue;
        };
        if name.text() != field_name {
            continue;
        }
        let start = text_offset(field.syntax().text_range().start());
        let indent = line_indent(source, start);
        source.insert_str(start, &format!("{indent}{attribute}\n"));
        return Ok(());
    }
    Err(format!("struct `{struct_name}` has no `{field_name}` field").into())
}

pub fn append_enum_variants(
    source: &mut String,
    enum_name: &str,
    variants: &str,
) -> Result<(), Box<dyn Error>> {
    let enumeration = parse_source(source)?
        .descendants()
        .filter_map(ast::Enum::cast)
        .find(|item| item.name().is_some_and(|it| it.text() == enum_name))
        .ok_or_else(|| format!("could not find enum `{enum_name}`"))?;
    let variants_list = enumeration
        .variant_list()
        .ok_or("enum has no variant list")?;
    let token = variants_list
        .r_curly_token()
        .ok_or("enum has no closing brace")?;
    let start = text_offset(token.text_range().start());
    source.insert_str(start, variants);
    Ok(())
}

pub fn add_enum_variant_attribute(
    source: &mut String,
    enum_name: &str,
    variant_name: &str,
    attribute: &str,
) -> Result<(), Box<dyn Error>> {
    let enumeration = parse_source(source)?
        .descendants()
        .filter_map(ast::Enum::cast)
        .find(|item| item.name().is_some_and(|it| it.text() == enum_name))
        .ok_or_else(|| format!("could not find enum `{enum_name}`"))?;
    let variants = enumeration
        .variant_list()
        .ok_or("enum has no variant list")?;
    for variant in variants.variants() {
        let Some(name) = variant.name() else {
            continue;
        };
        if name.text() != variant_name {
            continue;
        }
        let start = text_offset(variant.syntax().text_range().start());
        let indent = line_indent(source, start);
        source.insert_str(start, &format!("{indent}{attribute}\n"));
        return Ok(());
    }
    Err(format!("enum `{enum_name}` has no `{variant_name}` variant").into())
}

pub fn prepend_path_module(source: &mut String, visibility: Option<&str>, name: &str, path: &Path) {
    let visibility = visibility.map_or(String::new(), |visibility| format!("{visibility} "));
    source.insert_str(
        0,
        &format!(
            "#[path = {:?}]\n{visibility}mod {name};\n\n",
            path.to_string_lossy().into_owned()
        ),
    );
}

pub fn retarget_use_tree(
    source: &mut String,
    name: &str,
    path: &str,
    alias: &str,
) -> Result<(), Box<dyn Error>> {
    let file = parse_source(source)?;
    let matches = file
        .descendants()
        .filter_map(ast::UseTree::cast)
        .filter_map(|tree| {
            let path = tree.path()?;
            let segment = path.segment()?;
            let name_ref = segment.name_ref()?;
            (name_ref.text() == name && tree.rename().is_none()).then_some(tree)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [tree] => {
            if tree
                .syntax()
                .ancestors()
                .find_map(ast::UseTreeList::cast)
                .is_some()
            {
                let range = use_tree_removal_range(source, tree)?;
                source.replace_range(range, "");
                inject_use(source, &format!("{path} as {alias}"))?;
            } else {
                replace_text_range(
                    source,
                    tree.syntax().text_range(),
                    &format!("{path} as {alias}"),
                );
            }
            parse_source(source)?;
            Ok(())
        }
        [] => Err(format!("could not find use tree `{name}`").into()),
        _ => Err(format!("found multiple use trees `{name}`").into()),
    }
}

fn use_tree_removal_range(
    source: &str,
    tree: &ast::UseTree,
) -> Result<std::ops::Range<usize>, Box<dyn Error>> {
    let start = text_offset(tree.syntax().text_range().start());
    let end = text_offset(tree.syntax().text_range().end());
    let bytes = source.as_bytes();

    let mut after = end;
    while after < bytes.len() && bytes[after].is_ascii_whitespace() {
        after += 1;
    }
    if after < bytes.len() && bytes[after] == b',' {
        after += 1;
        while after < bytes.len() && bytes[after].is_ascii_whitespace() {
            after += 1;
        }
        return Ok(start..after);
    }

    let mut before = start;
    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before > 0 && bytes[before - 1] == b',' {
        return Ok(before - 1..end);
    }
    Ok(start..end)
}

pub fn append_function_params(
    source: &mut String,
    function: &str,
    params: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, function)?;
    let params_list = function
        .param_list()
        .ok_or("function has no parameter list")?;
    let token = params_list
        .r_paren_token()
        .ok_or("parameter list has no closing paren")?;
    let start = text_offset(token.text_range().start());
    source.insert_str(start, params);
    Ok(())
}

pub fn append_record_expr_fields_in_function(
    source: &mut String,
    function: &str,
    path_tail: &str,
    fields: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, function)?;
    for node in function.syntax().descendants() {
        let Some(record) = ast::RecordExpr::cast(node) else {
            continue;
        };
        let Some(path) = record.path() else {
            continue;
        };
        if !path.syntax().text().to_string().ends_with(path_tail) {
            continue;
        }
        let Some(field_list) = record.record_expr_field_list() else {
            continue;
        };
        let token = field_list
            .r_curly_token()
            .ok_or("record expression has no closing brace")?;
        let start = text_offset(token.text_range().start());
        source.insert_str(start, fields);
        return Ok(());
    }
    Err(format!("function `{function}` has no `{path_tail}` record expression").into())
}

pub fn replace_record_expr_field_in_function(
    source: &mut String,
    function: &str,
    path_tail: &str,
    field: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let function = find_function(source, function)?;
    for node in function.syntax().descendants() {
        let Some(record) = ast::RecordExpr::cast(node) else {
            continue;
        };
        let Some(path) = record.path() else {
            continue;
        };
        if !path.syntax().text().to_string().ends_with(path_tail) {
            continue;
        }
        let Some(field_list) = record.record_expr_field_list() else {
            continue;
        };
        for record_field in field_list.fields() {
            let Some(name) = record_field.name_ref() else {
                continue;
            };
            if name.text() != field {
                continue;
            }
            let Some(expr) = record_field.expr() else {
                return Err(format!("record field `{field}` has no expression").into());
            };
            replace_text_range(source, expr.syntax().text_range(), value);
            return Ok(());
        }
    }
    Err(format!("function `{function}` has no `{path_tail}.{field}` field").into())
}

fn find_function(source: &str, name: &str) -> Result<ast::Fn, Box<dyn Error>> {
    parse_source(source)?
        .descendants()
        .filter_map(ast::Fn::cast)
        .find(|function| function.name().is_some_and(|it| it.text() == name))
        .ok_or_else(|| format!("could not find function `{name}`").into())
}

fn find_struct(source: &str, name: &str) -> Result<ast::Struct, Box<dyn Error>> {
    parse_source(source)?
        .descendants()
        .filter_map(ast::Struct::cast)
        .find(|item| item.name().is_some_and(|it| it.text() == name))
        .ok_or_else(|| format!("could not find struct `{name}`").into())
}

fn parse_source(source: &str) -> Result<SyntaxNode, Box<dyn Error>> {
    let parsed = SourceFile::parse(source, Edition::CURRENT);
    let errors = parsed.errors();
    if !errors.is_empty() {
        return Err(format!("could not parse Rust source: {errors:?}").into());
    }
    Ok(parsed.syntax_node())
}

fn replace_text_range(source: &mut String, range: ra_ap_syntax::TextRange, replacement: &str) {
    source.replace_range(text_range(range), replacement);
}

fn text_range(range: ra_ap_syntax::TextRange) -> std::ops::Range<usize> {
    text_offset(range.start())..text_offset(range.end())
}

fn text_offset(size: ra_ap_syntax::TextSize) -> usize {
    u32::from(size) as usize
}

fn line_indent(source: &str, offset: usize) -> &str {
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let indent_len = source[line_start..offset]
        .chars()
        .take_while(|value| value.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    &source[line_start..line_start + indent_len]
}

fn first_use_index(source: &str) -> Option<usize> {
    let mut index = 0;
    for line in source.split_inclusive('\n') {
        if line.starts_with("use ") || line.starts_with("pub use ") {
            return Some(index);
        }
        index += line.len();
    }
    None
}

fn insertion_index_after_inner_attrs(source: &str) -> usize {
    let mut index = 0;
    for line in source.split_inclusive('\n') {
        if !(line.starts_with("#![") || line.starts_with("//!")) {
            return index;
        }
        index += line.len();
    }
    index
}

fn package_dir(package_name: &str, package: &LockedPackage) -> String {
    format!("{package_name}-{}", package.version)
}

fn archive_name(package_name: &str, package: &LockedPackage) -> String {
    format!("{}.crate", package_dir(package_name, package))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_use_before_existing_imports() {
        let mut source = String::from("#![allow(clippy::all)]\n\nuse std::path::Path;\n");

        inject_use(&mut source, "crate::patched::run_flycheck").unwrap();

        assert_eq!(
            source,
            "#![allow(clippy::all)]\n\nuse crate::patched::run_flycheck;\nuse std::path::Path;\n"
        );
    }
}
