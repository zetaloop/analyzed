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
    ast::{self, HasName, HasVisibility},
};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct LockedPackage {
    pub version: String,
    pub checksum: String,
}

pub fn prepare_bridge_package(
    package_name: &str,
    generated_dir: &str,
) -> Result<(PathBuf, LockedPackage), Box<dyn Error>> {
    let package = locked_package(package_name)?;
    let archive = registry_archive(package_name, &package)?;
    verify_archive_checksum(&archive, &package)?;

    let generated =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo")).join(generated_dir);
    unpack_crate_archive(package_name, &archive, &generated, &package)?;
    rewrite_lib_header(&generated.join("src/lib.rs"))?;

    println!("cargo:rerun-if-changed={}", archive.display());
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root().join("Cargo.lock").display()
    );

    Ok((generated, package))
}

fn registry_archive(
    package_name: &str,
    package: &LockedPackage,
) -> Result<PathBuf, Box<dyn Error>> {
    let cargo_home = cargo_home()?;
    if let Some(archive) = find_registry_archive(package_name, &cargo_home, package)? {
        return Ok(archive);
    }

    fetch_registry_packages()?;

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

fn fetch_registry_packages() -> Result<(), Box<dyn Error>> {
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

fn locked_package(package_name: &str) -> Result<LockedPackage, Box<dyn Error>> {
    let version = env::var("CARGO_PKG_VERSION")?;
    let lock = workspace_root().join("Cargo.lock");
    let checksum = locked_package_checksum(package_name, &lock, &version)?.ok_or_else(|| {
        format!(
            "could not find registry {package_name} {version} checksum in {}",
            lock.display()
        )
    })?;

    Ok(LockedPackage { version, checksum })
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

pub fn replace_once(
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

pub fn widen_visibility(
    source: &mut String,
    signature: &str,
    visibility: &str,
) -> Result<(), Box<dyn Error>> {
    replace_once(source, signature, &format!("{visibility} {signature}"))
}

pub fn rename_with_prefix(
    source: &mut String,
    signature: &str,
    name: &str,
    prefix: &str,
) -> Result<String, Box<dyn Error>> {
    let replacement = replace_identifier(signature, name, &format!("{prefix}{name}"))?;
    replace_once(source, signature, &replacement)?;
    Ok(replacement)
}

pub fn allow_dead_code(source: &mut String, signature: &str) -> Result<(), Box<dyn Error>> {
    replace_once(
        source,
        signature,
        &format!("#[allow(dead_code)]\n{signature}"),
    )
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

pub fn rename_method_calls_in_function(
    source: &mut String,
    function: &str,
    method: &str,
    replacement: &str,
) -> Result<usize, Box<dyn Error>> {
    let ranges = method_call_name_ranges(source, function, method)?;
    let count = ranges.len();
    for range in ranges.into_iter().rev() {
        source.replace_range(range, replacement);
    }
    Ok(count)
}

pub fn rename_last_method_call_in_function(
    source: &mut String,
    function: &str,
    method: &str,
    replacement: &str,
) -> Result<(), Box<dyn Error>> {
    let mut ranges = method_call_name_ranges(source, function, method)?;
    let Some(range) = ranges.pop() else {
        return Err(format!("function `{function}` has no `{method}` method call").into());
    };
    source.replace_range(range, replacement);
    Ok(())
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

fn find_function(source: &str, name: &str) -> Result<ast::Fn, Box<dyn Error>> {
    parse_source(source)?
        .descendants()
        .filter_map(ast::Fn::cast)
        .find(|function| function.name().is_some_and(|it| it.text() == name))
        .ok_or_else(|| format!("could not find function `{name}`").into())
}

fn method_call_name_ranges(
    source: &str,
    function: &str,
    method: &str,
) -> Result<Vec<std::ops::Range<usize>>, Box<dyn Error>> {
    let function = find_function(source, function)?;
    let ranges = function
        .syntax()
        .descendants()
        .filter_map(ast::MethodCallExpr::cast)
        .filter_map(|call| call.name_ref())
        .filter(|name| name.text() == method)
        .map(|name| text_range(name.syntax().text_range()))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        Err(format!("function `{function}` has no `{method}` method call").into())
    } else {
        Ok(ranges)
    }
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

fn replace_identifier(
    source: &str,
    name: &str,
    replacement: &str,
) -> Result<String, Box<dyn Error>> {
    let mut offset = 0;
    while let Some(index) = source[offset..].find(name).map(|index| offset + index) {
        let end = index + name.len();
        if is_identifier_boundary(source, index, end) {
            let mut source = source.to_owned();
            source.replace_range(index..end, replacement);
            return Ok(source);
        }
        offset = end;
    }

    Err(format!("could not find identifier `{name}` in source fragment:\n{source}").into())
}

fn is_identifier_boundary(source: &str, start: usize, end: usize) -> bool {
    let before = source[..start].chars().next_back();
    let after = source[end..].chars().next();

    before.is_none_or(|before| !is_identifier_char(before))
        && after.is_none_or(|after| !is_identifier_char(after))
}

fn is_identifier_char(value: char) -> bool {
    value == '_' || value.is_ascii_alphanumeric()
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

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("../..")
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
    fn widens_visibility() {
        let mut source = String::from("fn run_flycheck() {}\n");

        widen_visibility(&mut source, "fn run_flycheck()", "pub(crate)").unwrap();

        assert_eq!(source, "pub(crate) fn run_flycheck() {}\n");
    }

    #[test]
    fn renames_with_prefix() {
        let mut source = String::from("fn handle() {}\nfn handle_inner() {}\n");

        let renamed = rename_with_prefix(&mut source, "fn handle()", "handle", "_").unwrap();

        assert_eq!(renamed, "fn _handle()");
        assert_eq!(source, "fn _handle() {}\nfn handle_inner() {}\n");
    }

    #[test]
    fn adds_dead_code_allow() {
        let mut source = String::from("fn old_handle() {}\n");

        allow_dead_code(&mut source, "fn old_handle()").unwrap();

        assert_eq!(source, "#[allow(dead_code)]\nfn old_handle() {}\n");
    }

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
