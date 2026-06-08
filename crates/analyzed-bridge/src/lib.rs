use std::{
    env,
    error::Error,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use flate2::read::GzDecoder;
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
