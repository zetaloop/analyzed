use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use toml::{Table, Value, map::Map};

use analyzed_bridge as build_support;

const RA_PACKAGE: &str = "ra_ap_rust-analyzer";
const RA_REPOSITORY: &str = "rust-lang/rust-analyzer";

fn main() -> Result<(), Box<dyn Error>> {
    let (generated, package) =
        build_support::prepare_bridge_package(RA_PACKAGE, "ra_ap_rust_analyzer_bridge")?;
    let revision = package
        .git_revision
        .as_deref()
        .ok_or("ra_ap_rust-analyzer does not contain .cargo_vcs_info.json")?;
    let release = rust_analyzer_release(revision)?;
    verify_manifest_matches_bridge(&generated.join("Cargo.toml"))?;
    let generated_src = generated.join("src");
    patch_config_source(&generated_src.join("config.rs"))?;
    patch_discover_source(&generated_src.join("discover.rs"))?;
    patch_global_state_source(&generated_src.join("global_state.rs"))?;
    patch_main_loop_source(&generated_src.join("main_loop.rs"))?;
    patch_reload_source(&generated_src.join("reload.rs"))?;
    patch_flycheck_to_proto_source(&generated_src.join("diagnostics/flycheck_to_proto.rs"))?;
    patch_notification_source(&generated_src.join("handlers/notification.rs"))?;
    patch_test_tool_attributes(&generated_src)?;
    write_analyzed_root_module(
        &generated_src.join("analyzed_root.rs"),
        &generated_src.join("lib.rs"),
    )?;
    let slow_tests = generated.join("tests/slow-tests");
    patch_slow_tests(&slow_tests)?;
    let slow_tests_wrapper = write_slow_tests_wrapper(&slow_tests)?;
    println!(
        "cargo:rustc-env=ANALYZED_RA_CRATE_VERSION={}",
        package.version
    );
    println!(
        "cargo:rustc-env=ANALYZED_RA_RELEASE_VERSION={}",
        release.version
    );
    println!("cargo:rustc-env=ANALYZED_RA_COMMIT_HASH={revision}");
    println!(
        "cargo:rustc-env=ANALYZED_RA_VERSION={} {}",
        release.version,
        &revision[..8]
    );
    println!(
        "cargo:rustc-env=ANALYZED_RA_SLOW_TESTS={}",
        slow_tests_wrapper.display()
    );
    println!("cargo:rerun-if-env-changed=GITHUB_TOKEN");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

struct RustAnalyzerRelease {
    version: String,
}

fn rust_analyzer_release(revision: &str) -> Result<RustAnalyzerRelease, Box<dyn Error>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();

    for page in 1.. {
        let releases = github_get(
            &agent,
            &format!("/repos/{RA_REPOSITORY}/releases?per_page=100&page={page}"),
        )?;
        let releases = releases
            .as_array()
            .ok_or("GitHub releases response is not an array")?;

        for release in releases {
            let Some(body) = release.get("body").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if release_commit(body) != Some(revision) {
                continue;
            }
            let tag = release
                .get("tag_name")
                .and_then(serde_json::Value::as_str)
                .ok_or("matching rust-analyzer release has no tag_name")?;
            let version = release_version(body)
                .ok_or("matching rust-analyzer release has no extension version")?;
            verify_release_tag(&agent, tag, revision)?;
            return Ok(RustAnalyzerRelease {
                version: version.to_owned(),
            });
        }

        if releases.len() < 100 {
            break;
        }
    }

    Err(format!("could not find a rust-analyzer release for commit {revision}").into())
}

fn github_get(agent: &ureq::Agent, path: &str) -> Result<serde_json::Value, Box<dyn Error>> {
    let url = format!("https://api.github.com{path}");
    let mut request = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header(
            "User-Agent",
            format!("analyzed/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request.call()?;
    Ok(serde_json::from_str(
        &response.body_mut().read_to_string()?,
    )?)
}

fn release_commit(body: &str) -> Option<&str> {
    body.lines()
        .find(|line| line.starts_with("Commit: "))
        .and_then(|line| line.split_once("/commit/"))
        .and_then(|(_, revision)| revision.split(')').next())
}

fn release_version(body: &str) -> Option<&str> {
    let line = body.lines().find(|line| line.starts_with("Release: "))?;
    let (_, version) = line.rsplit_once(" (`")?;
    let (version, _) = version.split_once("`)")?;
    let mut parts = version.strip_prefix('v')?.split('.');
    let parts = (parts.next()?, parts.next()?, parts.next()?, parts.next());
    if parts.3.is_none()
        && [parts.0, parts.1, parts.2]
            .into_iter()
            .all(|part| part.parse::<u64>().is_ok())
    {
        Some(version)
    } else {
        None
    }
}

fn verify_release_tag(
    agent: &ureq::Agent,
    tag: &str,
    expected_revision: &str,
) -> Result<(), Box<dyn Error>> {
    let reference = github_get(agent, &format!("/repos/{RA_REPOSITORY}/git/ref/tags/{tag}"))?;
    let object = reference
        .get("object")
        .ok_or("rust-analyzer release tag has no object")?;
    let actual_revision = object
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .ok_or("rust-analyzer release tag has no object SHA")?;
    let object_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or("rust-analyzer release tag has no object type")?;
    if object_type != "commit" {
        return Err(format!("rust-analyzer release tag {tag} is not a lightweight tag").into());
    }
    if actual_revision != expected_revision {
        return Err(format!(
            "rust-analyzer release tag {tag} points to {actual_revision}, expected {expected_revision}"
        )
        .into());
    }
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
    let analyzed_global_state = owned_source_path("global_state.rs");
    let analyzed_main_loop = owned_source_path("main_loop.rs");
    let analyzed_reload = owned_source_path("reload.rs");
    let mut upstream_root = fs::read_to_string(lib_rs)?;
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
pub(crate) mod analyzed_global_state;

#[path = {:?}]
pub(crate) mod analyzed_main_loop;

#[path = {:?}]
pub(crate) mod analyzed_reload;

{upstream_root}

pub use analyzed_bridge::{{
    RUST_ANALYZER_COMMIT_HASH, RUST_ANALYZER_CRATE_VERSION, RUST_ANALYZER_RELEASE_VERSION,
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
        analyzed_global_state.to_string_lossy().into_owned(),
        analyzed_main_loop.to_string_lossy().into_owned(),
        analyzed_reload.to_string_lossy().into_owned()
    );
    fs::write(root_rs, source)?;
    println!("cargo:rerun-if-changed={}", analyzed_bridge.display());
    println!("cargo:rerun-if-changed={}", analyzed_global_state.display());
    println!("cargo:rerun-if-changed={}", analyzed_main_loop.display());
    println!("cargo:rerun-if-changed={}", analyzed_reload.display());

    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}

fn use_owned_handlers_module(
    source: &mut String,
    name: &str,
    path: PathBuf,
) -> Result<(), Box<dyn Error>> {
    let module_start = "mod handlers {\n";
    let insert_at = source
        .find(module_start)
        .map(|index| index + module_start.len())
        .ok_or("could not find handlers module")?;
    source.insert_str(
        insert_at,
        &format!(
            "    #[path = {:?}]\n    pub(crate) mod {name};\n",
            path.to_string_lossy().into_owned()
        ),
    );
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
        let function = guard
            .strip_prefix("fn ")
            .and_then(|value| value.strip_suffix("() {"))
            .ok_or("unexpected config test guard")?;
        build_support::add_function_attribute(
            &mut source,
            function,
            "#[ignore = \"regenerates files from the rust-analyzer source tree\"]",
        )?;
    }

    fs::write(config_rs, source)?;
    Ok(())
}

fn patch_discover_source(discover_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(discover_rs)?;
    build_support::add_enum_variant_attribute(
        &mut source,
        "DiscoverArgument",
        "Buildfile",
        "#[allow(dead_code)]",
    )?;
    fs::write(discover_rs, source)?;
    Ok(())
}

fn patch_global_state_source(global_state_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(global_state_rs)?;

    build_support::append_struct_fields(
        &mut source,
        "FetchWorkspaceResponse",
        "    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n",
    )?;
    build_support::add_struct_attribute(&mut source, "FetchWorkspaceResponse", "#[derive(Debug)]")?;
    build_support::append_struct_fields(
        &mut source,
        "GlobalState",
        "    pub(crate) analyzed_provider: crate::analyzed_bridge::SharedAnalyzerProvider,\n    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n",
    )?;
    build_support::append_struct_fields(
        &mut source,
        "GlobalStateSnapshot",
        "    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n",
    )?;
    build_support::widen_struct_field_visibility(
        &mut source,
        "GlobalStateSnapshot",
        "mem_docs",
        "pub(crate)",
    )?;
    build_support::add_struct_field_attribute(
        &mut source,
        "GlobalState",
        "last_gc_revision",
        "#[allow(dead_code)]",
    )?;

    build_support::rename_function(&mut source, "new", "new_analyzed")?;
    build_support::append_function_params(
        &mut source,
        "new_analyzed",
        ",\n        analyzed_provider: crate::analyzed_bridge::SharedAnalyzerProvider,\n        analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,\n        analyzed_workspaces: Vec<ProjectWorkspace>",
    )?;
    build_support::append_record_expr_fields_in_function(
        &mut source,
        "new_analyzed",
        "GlobalState",
        "            analyzed_provider,\n            analyzed_shared,\n",
    )?;
    build_support::replace_record_expr_field_in_function(
        &mut source,
        "new_analyzed",
        "GlobalState",
        "workspaces",
        "Arc::new(analyzed_workspaces)",
    )?;

    build_support::replace_record_expr_field_in_function(
        &mut source,
        "snapshot",
        "GlobalStateSnapshot",
        "analysis",
        "self.analyzed_shared.analysis()",
    )?;
    build_support::append_record_expr_fields_in_function(
        &mut source,
        "snapshot",
        "GlobalStateSnapshot",
        "            analyzed_shared: self.analyzed_shared.clone(),\n",
    )?;
    build_support::extract_method(
        &mut source,
        "target_spec_for_file",
        build_support::ExtractSelector::LetBinding("path"),
        0,
        build_support::ExtractRange::Initializer {
            return_ty: "vfs::VfsPath",
        },
        build_support::ExtractedMethod {
            name: "target_spec_vfs_path",
            receiver: Some("&self"),
            params: &[build_support::MethodParam {
                name: "file_id",
                ty: "FileId",
            }],
            args: &["file_id"],
        },
    )?;
    build_support::rename_function(&mut source, "target_spec_vfs_path", "_target_spec_vfs_path")?;
    build_support::allow_dead_code_for_function(&mut source, "_target_spec_vfs_path")?;
    for name in [
        "process_changes",
        "url_to_file_id",
        "file_id_to_url",
        "vfs_path_to_file_id",
        "file_line_index",
        "file_version",
        "anchored_path",
        "file_id_to_file_path",
        "file_exists",
    ] {
        let replacement = format!("_{name}");
        build_support::rename_function(&mut source, name, &replacement)?;
        build_support::allow_dead_code_for_function(&mut source, &replacement)?;
    }
    build_support::widen_function_visibility(&mut source, "enqueue_workspace_fetch", "pub(crate)")?;

    fs::write(global_state_rs, source)?;
    Ok(())
}

fn patch_main_loop_source(main_loop_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(main_loop_rs)?;
    let main_loop_export = "pub use crate::analyzed_main_loop::main_loop;\n";
    let export_at = source
        .find("use std::")
        .ok_or("main_loop source has no std use")?;
    source.insert_str(export_at, main_loop_export);

    build_support::rename_function(&mut source, "main_loop", "_main_loop")?;
    build_support::allow_dead_code_for_function(&mut source, "_main_loop")?;
    build_support::widen_function_visibility(&mut source, "_main_loop", "pub(crate)")?;
    build_support::allow_dead_code_for_function(&mut source, "run")?;
    build_support::widen_enum_visibility(&mut source, "Event", "pub(crate)")?;
    build_support::append_enum_variants(
        &mut source,
        "Task",
        "    AnalyzedFetchWorkspace(FetchWorkspaceResponse),\n",
    )?;
    build_support::add_enum_variant_attribute(
        &mut source,
        "DiscoverProjectParam",
        "Buildfile",
        "#[allow(dead_code)]",
    )?;

    for name in [
        "handle_event",
        "update_diagnostics",
        "update_tests",
        "handle_task",
    ] {
        let replacement = format!("_{name}");
        build_support::rename_function(&mut source, name, &replacement)?;
    }

    build_support::extract_method(
        &mut source,
        "_update_diagnostics",
        build_support::ExtractSelector::LetBinding("subscriptions"),
        0,
        build_support::ExtractRange::TailToBlockEnd,
        build_support::ExtractedMethod {
            name: "spawn_native_diagnostics",
            receiver: Some("&mut self"),
            params: &[
                build_support::MethodParam {
                    name: "generation",
                    ty: "DiagnosticsGeneration",
                },
                build_support::MethodParam {
                    name: "subscriptions",
                    ty: "std::sync::Arc<[FileId]>",
                },
            ],
            args: &["generation", "subscriptions"],
        },
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_update_diagnostics")?;
    build_support::extract_method(
        &mut source,
        "_update_tests",
        build_support::ExtractSelector::LetBinding("subscriptions"),
        0,
        build_support::ExtractRange::TailToBlockEnd,
        build_support::ExtractedMethod {
            name: "spawn_discover_tests",
            receiver: Some("&mut self"),
            params: &[build_support::MethodParam {
                name: "subscriptions",
                ty: "Vec<FileId>",
            }],
            args: &["subscriptions"],
        },
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_update_tests")?;

    build_support::extract_method(
        &mut source,
        "_handle_event",
        build_support::ExtractSelector::TopLevelMethodCall("trigger_garbage_collection"),
        0,
        build_support::ExtractRange::StatementSequence { len: 1 },
        build_support::ExtractedMethod {
            name: "mark_prime_caches_gc",
            receiver: Some("&mut self"),
            params: &[],
            args: &[],
        },
    )?;
    build_support::rename_function(&mut source, "mark_prime_caches_gc", "_mark_prime_caches_gc")?;
    build_support::allow_dead_code_for_function(&mut source, "_mark_prime_caches_gc")?;

    build_support::extract_method(
        &mut source,
        "_handle_event",
        build_support::ExtractSelector::TopLevelMethodCall("trigger_garbage_collection"),
        0,
        build_support::ExtractRange::StatementSequence { len: 1 },
        build_support::ExtractedMethod {
            name: "mark_gc_when_idle",
            receiver: Some("&mut self"),
            params: &[],
            args: &[],
        },
    )?;
    build_support::rename_function(&mut source, "mark_gc_when_idle", "_mark_gc_when_idle")?;
    build_support::allow_dead_code_for_function(&mut source, "_mark_gc_when_idle")?;

    build_support::extract_method(
        &mut source,
        "_handle_event",
        build_support::ExtractSelector::ForLoopBinding("file_id"),
        0,
        build_support::ExtractRange::Body,
        build_support::ExtractedMethod {
            name: "publish_changed_diagnostics",
            receiver: Some("&mut self"),
            params: &[build_support::MethodParam {
                name: "file_id",
                ty: "FileId",
            }],
            args: &["file_id"],
        },
    )?;
    build_support::rename_function(
        &mut source,
        "publish_changed_diagnostics",
        "_publish_changed_diagnostics",
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_publish_changed_diagnostics")?;

    build_support::extract_method(
        &mut source,
        "handle_flycheck_msg",
        build_support::ExtractSelector::ForLoopBinding("diag"),
        0,
        build_support::ExtractRange::Body,
        build_support::ExtractedMethod {
            name: "record_flycheck_diagnostic",
            receiver: Some("&mut self"),
            params: &[
                build_support::MethodParam {
                    name: "id",
                    ty: "usize",
                },
                build_support::MethodParam {
                    name: "generation",
                    ty: "DiagnosticsGeneration",
                },
                build_support::MethodParam {
                    name: "package_id",
                    ty: "Option<crate::flycheck::PackageSpecifier>",
                },
                build_support::MethodParam {
                    name: "diag",
                    ty: "crate::diagnostics::flycheck_to_proto::MappedRustDiagnostic",
                },
            ],
            args: &["id", "generation", "package_id.clone()", "diag"],
        },
    )?;
    build_support::rename_function(
        &mut source,
        "record_flycheck_diagnostic",
        "_record_flycheck_diagnostic",
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_record_flycheck_diagnostic")?;

    build_support::append_record_expr_fields_in_function(
        &mut source,
        "_handle_task",
        "FetchWorkspaceResponse",
        ", analyzed_shared: self.analyzed_shared.clone()",
    )?;
    build_support::rename_path_root_in_function(
        &mut source,
        "_handle_task",
        "Task",
        "UpstreamTask",
    )?;
    build_support::inject_use(&mut source, "self::analyzed_session::UpstreamTask")?;

    let analyzed_session = owned_source_path("analyzed_session.rs");
    source.push_str(&format!(
        "\n#[path = {:?}]\npub(crate) mod analyzed_session;\n",
        analyzed_session.to_string_lossy().into_owned()
    ));

    fs::write(main_loop_rs, source)?;
    Ok(())
}

fn patch_reload_source(reload_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(reload_rs)?;

    for name in [
        "update_configuration",
        "fetch_workspaces",
        "fetch_build_data",
        "fetch_proc_macros",
        "recreate_crate_graph",
    ] {
        let replacement = format!("_{name}");
        build_support::rename_function(&mut source, name, &replacement)?;
        build_support::allow_dead_code_for_function(&mut source, &replacement)?;
    }

    build_support::widen_function_visibility(&mut source, "reload_flycheck", "pub(crate)")?;

    build_support::add_record_pattern_rest(
        &mut source,
        "switch_workspaces",
        "FetchWorkspaceResponse",
    )?;
    build_support::extract_method(
        &mut source,
        "switch_workspaces",
        build_support::ExtractSelector::TopLevelMethodCall("for_each"),
        0,
        build_support::ExtractRange::StatementSequence { len: 1 },
        build_support::ExtractedMethod {
            name: "show_workspace_msrv_warnings",
            receiver: Some("&mut self"),
            params: &[],
            args: &[],
        },
    )?;
    build_support::rename_function(
        &mut source,
        "show_workspace_msrv_warnings",
        "_show_workspace_msrv_warnings",
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_show_workspace_msrv_warnings")?;
    build_support::extract_method(
        &mut source,
        "switch_workspaces",
        build_support::ExtractSelector::TopLevelMethodCall("recreate_crate_graph"),
        3,
        build_support::ExtractRange::StatementSequence { len: 1 },
        build_support::ExtractedMethod {
            name: "recreate_crate_graph_after_shared_reload",
            receiver: Some("&mut self"),
            params: &[
                build_support::MethodParam {
                    name: "cause",
                    ty: "String",
                },
                build_support::MethodParam {
                    name: "switching_from_empty_workspace",
                    ty: "bool",
                },
            ],
            args: &["cause", "switching_from_empty_workspace"],
        },
    )?;
    build_support::rename_function(
        &mut source,
        "recreate_crate_graph_after_shared_reload",
        "_recreate_crate_graph_after_shared_reload",
    )?;
    build_support::allow_dead_code_for_function(
        &mut source,
        "_recreate_crate_graph_after_shared_reload",
    )?;

    fs::write(reload_rs, source)?;
    Ok(())
}

fn patch_flycheck_to_proto_source(flycheck_to_proto_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(flycheck_to_proto_rs)?;

    build_support::rename_function(&mut source, "location", "_location")?;
    build_support::allow_dead_code_for_function(&mut source, "_location")?;
    build_support::inject_use(&mut source, "self::analyzed_flycheck_location::location")?;
    let analyzed_location = owned_source_path("diagnostics/flycheck_location.rs");
    source.push_str(&format!(
        "\n#[path = {:?}]\nmod analyzed_flycheck_location;\n",
        analyzed_location.to_string_lossy().into_owned()
    ));
    println!("cargo:rerun-if-changed={}", analyzed_location.display());
    fs::write(flycheck_to_proto_rs, source)?;
    Ok(())
}

fn patch_notification_source(notification_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(notification_rs)?;

    build_support::rename_function(&mut source, "run_flycheck", "_run_flycheck")?;
    build_support::allow_dead_code_for_function(&mut source, "_run_flycheck")?;
    build_support::inject_pub_use(
        &mut source,
        "crate::handlers::analyzed_notification::run_flycheck",
    )?;
    build_support::rename_function(
        &mut source,
        "handle_did_save_text_document",
        "_handle_did_save_text_document",
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_handle_did_save_text_document")?;
    build_support::inject_pub_use(
        &mut source,
        "crate::handlers::analyzed_notification::handle_did_save_text_document",
    )?;

    fs::write(notification_rs, source)?;
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
            "use crate::analyzed_slow_tests::skip_slow_tests;\n",
        )
        .replace(
            r#".replace("C:\\", "/c:/").replace('\\', "/")"#,
            ".analyzed_uri_path()",
        );
    if source.contains(".analyzed_uri_path()") {
        build_support::inject_use(&mut source, "crate::analyzed_slow_tests::AnalyzedUriPath")?;
    }
    fs::write(path, source)?;
    Ok(())
}

fn write_slow_tests_wrapper(slow_tests: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let analyzed_slow_tests = owned_source_path("slow_tests.rs");
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
            "#[path = {:?}]\nmod analyzed_slow_tests;\n#[path = {:?}]\nmod cli;\n#[path = {:?}]\nmod flycheck;\n#[path = {:?}]\nmod ratoml;\n#[path = {:?}]\nmod support;\n#[path = {:?}]\nmod testdir;\ninclude!({:?});\n",
            analyzed_slow_tests.to_string_lossy().into_owned(),
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
    println!("cargo:rerun-if-changed={}", analyzed_slow_tests.display());
    Ok(wrapper_rs)
}
