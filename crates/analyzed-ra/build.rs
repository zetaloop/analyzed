use std::{
    env,
    error::Error,
    fs,
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
    patch_config_source(&generated_src.join("config.rs"))?;
    patch_discover_source(&generated_src.join("discover.rs"))?;
    patch_flycheck_to_proto_source(&generated_src.join("diagnostics/flycheck_to_proto.rs"))?;
    patch_notification_source(&generated_src.join("handlers/notification.rs"))?;
    patch_dispatch_source(&generated_src.join("handlers/dispatch.rs"))?;
    patch_test_tool_attributes(&generated_src)?;
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
    let analyzed_flycheck_to_proto = owned_source_path("diagnostics/flycheck_to_proto.rs");
    let mut upstream_root = fs::read_to_string(lib_rs)?;
    use_owned_module(
        &mut upstream_root,
        "global_state",
        owned_source_path("global_state.rs"),
    )?;
    use_owned_module(
        &mut upstream_root,
        "main_loop",
        owned_source_path("main_loop.rs"),
    )?;
    use_owned_module(&mut upstream_root, "reload", owned_source_path("reload.rs"))?;
    use_owned_handlers_module(
        &mut upstream_root,
        "analyzed_dispatch",
        owned_source_path("handlers/dispatch.rs"),
    )?;
    use_owned_handlers_module(
        &mut upstream_root,
        "analyzed_notification",
        owned_source_path("handlers/notification.rs"),
    )?;
    let source = format!(
        r#"
#[path = {:?}]
pub mod analyzed_bridge;

#[path = {:?}]
pub(crate) mod analyzed_flycheck_to_proto;

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
        analyzed_bridge.to_string_lossy().into_owned(),
        analyzed_flycheck_to_proto.to_string_lossy().into_owned()
    );
    fs::write(root_rs, source)?;
    println!("cargo:rerun-if-changed={}", analyzed_bridge.display());
    println!(
        "cargo:rerun-if-changed={}",
        analyzed_flycheck_to_proto.display()
    );

    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}

fn use_owned_module(source: &mut String, name: &str, path: PathBuf) -> Result<(), Box<dyn Error>> {
    let declaration = format!("mod {name};");
    replace_once(
        source,
        &declaration,
        &format!(
            "#[path = {:?}]\nmod {name};",
            path.to_string_lossy().into_owned()
        ),
    )?;
    println!("cargo:rerun-if-changed={}", path.display());
    Ok(())
}

fn use_owned_handlers_module(
    source: &mut String,
    name: &str,
    path: PathBuf,
) -> Result<(), Box<dyn Error>> {
    replace_once(
        source,
        "mod handlers {\n",
        &format!(
            "mod handlers {{\n    #[path = {:?}]\n    pub(crate) mod {name};\n",
            path.to_string_lossy().into_owned()
        ),
    )?;
    println!("cargo:rerun-if-changed={}", path.display());
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

fn patch_flycheck_to_proto_source(flycheck_to_proto_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(flycheck_to_proto_rs)?;

    build_support::rename_with_prefix(&mut source, "fn location(", "location", "_")?;
    build_support::allow_dead_code(&mut source, "fn _location(")?;
    build_support::inject_use(&mut source, "crate::analyzed_flycheck_to_proto::location")?;
    replace_once(
        &mut source,
        "    use crate::{config::Config, global_state::GlobalState};\n",
        "    use crate::analyzed_flycheck_to_proto::test_global_state;\n",
    )?;
    replace_once(
        &mut source,
        "        let state = GlobalState::new(\n            sender,\n            Config::new(\n                workspace_root.to_path_buf(),\n                ClientCapabilities::default(),\n                Vec::new(),\n                None,\n            ),\n        );\n",
        "        let state = test_global_state(\n            sender,\n            workspace_root.to_path_buf(),\n            ClientCapabilities::default(),\n        );\n",
    )?;

    fs::write(flycheck_to_proto_rs, source)?;
    Ok(())
}

fn patch_notification_source(notification_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(notification_rs)?;

    build_support::rename_with_prefix(
        &mut source,
        "fn run_flycheck(state: &mut GlobalState, vfs_path: VfsPath) -> bool",
        "run_flycheck",
        "_",
    )?;
    build_support::allow_dead_code(
        &mut source,
        "fn _run_flycheck(state: &mut GlobalState, vfs_path: VfsPath) -> bool",
    )?;
    build_support::inject_use(
        &mut source,
        "crate::handlers::analyzed_notification::run_flycheck",
    )?;
    build_support::rename_with_prefix(
        &mut source,
        "pub(crate) fn handle_did_save_text_document(",
        "handle_did_save_text_document",
        "_",
    )?;
    build_support::allow_dead_code(&mut source, "pub(crate) fn _handle_did_save_text_document(")?;
    build_support::inject_use(
        &mut source,
        "crate::handlers::analyzed_notification::handle_did_save_text_document",
    )?;
    replace_once(
        &mut source,
        "use crate::handlers::analyzed_notification::run_flycheck;\n",
        "pub(crate) use crate::handlers::analyzed_notification::run_flycheck;\n",
    )?;
    replace_once(
        &mut source,
        "use crate::handlers::analyzed_notification::handle_did_save_text_document;\n",
        "pub(crate) use crate::handlers::analyzed_notification::handle_did_save_text_document;\n",
    )?;

    fs::write(notification_rs, source)?;
    Ok(())
}

fn patch_dispatch_source(dispatch_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(dispatch_rs)?;

    build_support::rename_with_prefix(
        &mut source,
        "fn on_with_thread_intent<const RUSTFMT: bool, const ALLOW_RETRYING: bool, R>(",
        "on_with_thread_intent",
        "_",
    )?;
    build_support::allow_dead_code(
        &mut source,
        "fn _on_with_thread_intent<const RUSTFMT: bool, const ALLOW_RETRYING: bool, R>(",
    )?;
    build_support::widen_visibility(&mut source, "fn parse<R>(&mut self)", "pub(crate)")?;
    build_support::widen_visibility(&mut source, "enum HandlerCancelledError {", "pub(crate)")?;
    build_support::widen_visibility(
        &mut source,
        "fn thread_result_to_response<R>(",
        "pub(crate)",
    )?;

    fs::write(dispatch_rs, source)?;
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
