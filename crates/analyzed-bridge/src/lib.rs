use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use r#override::{DependencyKind, Package, check_target};

pub fn prepare_bridge_package(
    package_name: &str,
    generated_dir: &str,
    included_roots: &[&str],
    replacements: &[&str],
) -> Result<(PathBuf, Option<String>), Box<dyn Error>> {
    let bridge = Package::current()?;
    let upstream = bridge.dependency(package_name)?;
    let replacements = replacements
        .iter()
        .map(|&alias| Ok((alias, bridge.dependency(alias)?)))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    bridge.check_dependencies(&upstream, DependencyKind::Normal, &replacements)?;
    bridge.check_features(&upstream, &replacements)?;
    bridge.check_lints(&upstream)?;
    check_target(upstream.library()?, bridge.library()?)?;

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let mut sources = upstream.prepare(out.join(generated_dir))?;
    sources.include(upstream.library()?)?;
    for root in included_roots {
        let target = upstream
            .data()
            .targets
            .iter()
            .find(|target| {
                target
                    .src_path
                    .as_std_path()
                    .strip_prefix(upstream.directory())
                    .is_ok_and(|path| path == Path::new(root))
            })
            .ok_or_else(|| format!("upstream has no target at {root}"))?;
        sources.include(target)?;
    }
    let revision = crate_git_revision(sources.directory())?;
    Ok((sources.directory().to_owned(), revision))
}

// rust-analyzer's crates.io workflow rewrites its crate name in every Rust source file.
pub fn restore_rust_analyzer_source(source_dir: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(source_dir)? {
        let path = entry?.path();
        if path.is_dir() {
            restore_rust_analyzer_source(&path)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path)?;
            let restored = source.replace("ra_ap_rust_analyzer", "rust_analyzer");
            if restored != source {
                fs::write(path, restored)?;
            }
        }
    }
    Ok(())
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
