use std::{
    env,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use analyzed_bridge as build_support;
use analyzed_bridge::ast;

const RA_PACKAGE: &str = "ra_ap_rust-analyzer";
const RA_REPOSITORY: &str = "rust-lang/rust-analyzer";

fn main() -> Result<(), Box<dyn Error>> {
    let (generated, package) = build_support::prepare_bridge_package(
        RA_PACKAGE,
        "ra_ap_rust_analyzer_bridge",
        &["tests/slow-tests/main.rs"],
    )?;
    build_support::restore_rust_analyzer_source(&generated)?;
    let revision = package
        .git_revision
        .as_deref()
        .ok_or("ra_ap_rust-analyzer does not contain .cargo_vcs_info.json")?;
    let pinned = pinned_upstream("release")?;
    let pinned_tag = pinned_upstream("tag")?;
    let release = if offline_build() {
        pinned
    } else {
        match rust_analyzer_release(revision) {
            Ok((release, tag)) => {
                if release != pinned {
                    return Err(format!(
                        "[package.metadata.upstream] release is {pinned}, but the rust-analyzer \
                         release for commit {revision} is {release}"
                    )
                    .into());
                }
                if tag != pinned_tag {
                    return Err(format!(
                        "[package.metadata.upstream] tag is {pinned_tag}, but the rust-analyzer \
                         release tag for commit {revision} is {tag}"
                    )
                    .into());
                }
                release
            }
            Err(error) if error.is::<GithubUnavailable>() => {
                println!(
                    "cargo:warning=could not verify the pinned upstream release {pinned}: {error}"
                );
                pinned
            }
            Err(error) => return Err(error),
        }
    };
    let generated_src = generated.join("src");
    patch_config_source(&generated_src.join("config.rs"))?;
    patch_discover_source(&generated_src.join("discover.rs"))?;
    patch_diagnostics_source(&generated_src.join("diagnostics.rs"))?;
    patch_global_state_source(&generated_src.join("global_state.rs"))?;
    patch_main_loop_source(&generated_src.join("main_loop.rs"))?;
    patch_op_queue_source(&generated_src.join("op_queue.rs"))?;
    patch_reload_source(&generated_src.join("reload.rs"))?;
    patch_session_source(&generated_src.join("session.rs"))?;
    patch_task_pool_source(&generated_src.join("task_pool.rs"))?;
    patch_flycheck_to_proto_source(&generated_src.join("diagnostics/flycheck_to_proto.rs"))?;
    patch_dispatch_source(&generated_src.join("handlers/dispatch.rs"))?;
    patch_notification_source(&generated_src.join("handlers/notification.rs"))?;
    patch_request_source(&generated_src.join("handlers/request.rs"))?;
    patch_driver_source(&generated_src.join("bin/main.rs"))?;
    write_root_module(
        &generated_src.join("root.rs"),
        &generated_src.join("lib.rs"),
    )?;
    let slow_tests = generated.join("tests/slow-tests");
    patch_slow_tests(&slow_tests)?;
    write_slow_tests_wrapper(&slow_tests)?;
    println!("cargo:rustc-env=ANALYZED_RA_RELEASE_VERSION={}", release);
    println!("cargo:rustc-env=ANALYZED_RA_COMMIT_HASH={revision}");
    println!("cargo:rerun-if-env-changed=GITHUB_TOKEN");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn pinned_upstream(key: &str) -> Result<String, Box<dyn Error>> {
    let manifest_path = Path::new(&env::var("CARGO_MANIFEST_DIR")?).join("Cargo.toml");
    let manifest: toml::Table = toml::from_str(&fs::read_to_string(manifest_path)?)?;
    manifest
        .get("package")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("upstream"))
        .and_then(|value| value.get(key))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("Cargo.toml lacks a [package.metadata.upstream] {key}").into())
}

fn offline_build() -> bool {
    env::var_os("DOCS_RS").is_some()
        || env::var("CARGO_NET_OFFLINE").is_ok_and(|value| value == "true")
}

#[derive(Debug)]
struct GithubUnavailable(String);

impl fmt::Display for GithubUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for GithubUnavailable {}

fn rust_analyzer_release(revision: &str) -> Result<(String, String), Box<dyn Error>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();

    let tag = rust_analyzer_release_tag(&agent, revision)?;
    let runs = github_get(
        &agent,
        &format!(
            "/repos/{RA_REPOSITORY}/actions/workflows/release.yaml/runs\
             ?head_sha={revision}&branch=release&status=success"
        ),
    )?;
    let numbers = runs
        .get("workflow_runs")
        .and_then(serde_json::Value::as_array)
        .ok_or("GitHub workflow runs response has no workflow_runs array")?
        .iter()
        .filter_map(|run| run.get("run_number")?.as_u64())
        .collect::<Vec<_>>();

    match numbers.as_slice() {
        [number] => Ok((format!("v0.3.{number}"), tag)),
        [] => Err(GithubUnavailable(format!(
            "no release workflow run records commit {revision}; GitHub retains run history \
             for about 400 days"
        ))
        .into()),
        numbers => Err(format!(
            "multiple release workflow runs record commit {revision}: {}",
            numbers
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into()),
    }
}

fn rust_analyzer_release_tag(
    agent: &ureq::Agent,
    revision: &str,
) -> Result<String, Box<dyn Error>> {
    let refs = github_get(
        agent,
        &format!("/repos/{RA_REPOSITORY}/git/matching-refs/tags/"),
    )?;
    let refs = refs
        .as_array()
        .ok_or("GitHub matching refs response is not an array")?;
    let mut tags = refs
        .iter()
        .filter_map(|reference| {
            let object = reference.get("object")?;
            if object.get("type")?.as_str()? != "commit" {
                return None;
            }
            if object.get("sha")?.as_str()? != revision {
                return None;
            }
            reference.get("ref")?.as_str()?.strip_prefix("refs/tags/")
        })
        .filter(|tag| *tag != "nightly")
        .collect::<Vec<_>>();
    tags.sort();

    match tags.as_slice() {
        [tag] => Ok((*tag).to_owned()),
        [] => Err(format!("no rust-analyzer release tag points to commit {revision}").into()),
        tags => Err(format!(
            "multiple rust-analyzer release tags point to commit {revision}: {}",
            tags.join(", ")
        )
        .into()),
    }
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
    let mut response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(403 | 429)) if env::var_os("GITHUB_TOKEN").is_none() => {
            return Err(GithubUnavailable(format!(
                "GitHub API request to {path} was rate limited (60 requests/hour unauthenticated); \
                 set GITHUB_TOKEN to raise the limit"
            ))
            .into());
        }
        Err(error @ ureq::Error::StatusCode(_)) => return Err(error.into()),
        Err(error) => {
            return Err(
                GithubUnavailable(format!("GitHub API request to {path} failed: {error}")).into(),
            );
        }
    };
    Ok(serde_json::from_str(
        &response.body_mut().read_to_string()?,
    )?)
}

fn write_root_module(root_rs: &Path, lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let shared_analyzer = owned_source_path("shared_analyzer.rs");
    let shared_global_state = owned_source_path("global_state.rs");
    let shared_reload = owned_source_path("reload.rs");
    let shared_notification = owned_source_path("handlers/notification.rs");
    let upstream_root = fs::read_to_string(lib_rs)?;
    let source = format!(
        r#"
#[path = {:?}]
pub mod shared_analyzer;

#[path = {:?}]
pub(crate) mod shared_global_state;

#[path = {:?}]
pub(crate) mod shared_reload;

#[path = {:?}]
pub(crate) mod shared_notification;

{upstream_root}

#[path = {:?}]
pub mod driver;

pub use shared_analyzer::{{
    RUST_ANALYZER_VERSION,
    SharedAnalyzerBackendKey, SharedAnalyzerCargoConfigKey, SharedAnalyzerDatabaseConfigKey,
    SharedAnalyzerBackendSnapshot, SharedAnalyzerLoadKey,
    SharedAnalyzerProcMacroServerKey, SharedAnalyzerRegistry,
    SharedAnalyzerWorldKey, SharedAnalyzerViewKey, WorkspaceSummary,
    run_shared_rust_analyzer_lsp_session, run_shared_rust_analyzer_lsp_session_with_config,
    shared_analyzer_registry,
}};
"#,
        shared_analyzer.to_string_lossy().into_owned(),
        shared_global_state.to_string_lossy().into_owned(),
        shared_reload.to_string_lossy().into_owned(),
        shared_notification.to_string_lossy().into_owned(),
        lib_rs
            .with_file_name("bin/main.rs")
            .to_string_lossy()
            .into_owned()
    );
    fs::write(root_rs, source)?;
    println!("cargo:rerun-if-changed={}", shared_analyzer.display());
    println!("cargo:rerun-if-changed={}", shared_global_state.display());
    println!("cargo:rerun-if-changed={}", shared_reload.display());
    println!("cargo:rerun-if-changed={}", shared_notification.display());

    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}

fn patch_config_source(config_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(config_rs)?;

    for guard in [
        "fn generate_package_json_config() {",
        "fn generate_config_documentation() {",
    ] {
        let function = guard
            .strip_prefix("fn ")
            .and_then(|value| value.strip_suffix("() {"))
            .ok_or("unexpected config test guard")?;
        build_support::add_attr::<ast::Fn>(
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
    build_support::add_attr::<ast::Variant>(
        &mut source,
        "DiscoverArgument::Buildfile",
        "#[allow(dead_code)]",
    )?;
    fs::write(discover_rs, source)?;
    Ok(())
}

fn patch_diagnostics_source(diagnostics_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(diagnostics_rs)?;
    build_support::rename::<ast::Fn>(
        &mut source,
        "fetch_native_diagnostics",
        "_fetch_native_diagnostics",
    )?;
    build_support::add_use(
        &mut source,
        Some("pub(crate)"),
        "crate::main_loop::session::fetch_native_diagnostics",
    )?;
    fs::write(diagnostics_rs, source)?;
    Ok(())
}

fn patch_global_state_source(global_state_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(global_state_rs)?;

    build_support::append::<ast::Struct>(
        &mut source,
        "FetchWorkspaceResponse",
        &[
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "shared",
                ty: "crate::shared_analyzer::SharedAnalyzerRuntime",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "reload_id",
                ty: "Option<u64>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "adopted",
                ty: "bool",
            },
        ],
    )?;
    build_support::append::<ast::Struct>(
        &mut source,
        "FetchBuildDataResponse",
        &[
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "rebuild_id",
                ty: "Option<u64>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "reload",
                ty: "bool",
            },
        ],
    )?;
    build_support::add_attr::<ast::Struct>(
        &mut source,
        "FetchWorkspaceResponse",
        "#[derive(Debug)]",
    )?;
    build_support::append::<ast::Struct>(
        &mut source,
        "GlobalState",
        &[
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "shared",
                ty: "crate::shared_analyzer::SharedAnalyzerRuntime",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "reload_workspace",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "rebuild_proc_macros",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "rebuild_queued",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "rebuilding_proc_macros",
                ty: "Option<u64>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "proc_macro_rebuild_id",
                ty: "u64",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "rebuild_response_current",
                ty: "Option<u64>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "build_data_response_current",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "build_data_adoption",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "build_data_rebuild_id",
                ty: "Option<u64>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "build_data_reload",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "build_data_generation",
                ty: "u64",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "build_data_operation",
                ty: "Option<crate::shared_analyzer::SharedAnalyzerOperationToken>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "proc_macro_operation",
                ty: "Option<crate::shared_analyzer::SharedAnalyzerOperationToken>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "reload_pending",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "proc_macro_clients_failed",
                ty: "bool",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "workspace_reload_id",
                ty: "u64",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "handled_workspace_reload",
                ty: "Option<u64>",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "workspace_adoption",
                ty: "Option<Arc<Vec<ProjectWorkspace>>>",
            },
        ],
    )?;
    build_support::append::<ast::Struct>(
        &mut source,
        "GlobalStateSnapshot",
        &[build_support::Field {
            vis: Some("pub(crate)"),
            name: "shared",
            ty: "crate::shared_analyzer::SharedAnalyzerRuntime",
        }],
    )?;
    for field in ["mem_docs", "vfs", "minicore"] {
        build_support::set_visibility::<ast::RecordField>(
            &mut source,
            &format!("GlobalStateSnapshot::{field}"),
            "pub(crate)",
        )?;
    }
    build_support::add_attr::<ast::RecordField>(
        &mut source,
        "GlobalState::last_gc_revision",
        "#[allow(dead_code)]",
    )?;

    build_support::rename::<ast::Fn>(&mut source, "new", "new_with_shared")?;
    build_support::append::<ast::Fn>(
        &mut source,
        "new_with_shared",
        &[
            build_support::Param {
                name: "shared",
                ty: "crate::shared_analyzer::SharedAnalyzerRuntime",
            },
            build_support::Param {
                name: "workspaces",
                ty: "Vec<ProjectWorkspace>",
            },
        ],
    )?;
    build_support::append_record_fields(
        &mut source,
        "new_with_shared",
        "GlobalState",
        &[
            build_support::FieldInit {
                name: "shared",
                value: None,
            },
            build_support::FieldInit {
                name: "reload_workspace",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "rebuild_proc_macros",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "rebuild_queued",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "rebuilding_proc_macros",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "proc_macro_rebuild_id",
                value: Some("0"),
            },
            build_support::FieldInit {
                name: "rebuild_response_current",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "build_data_response_current",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "build_data_adoption",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "build_data_rebuild_id",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "build_data_reload",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "build_data_generation",
                value: Some("0"),
            },
            build_support::FieldInit {
                name: "build_data_operation",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "proc_macro_operation",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "reload_pending",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "proc_macro_clients_failed",
                value: Some("false"),
            },
            build_support::FieldInit {
                name: "workspace_reload_id",
                value: Some("0"),
            },
            build_support::FieldInit {
                name: "handled_workspace_reload",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "workspace_adoption",
                value: Some("None"),
            },
        ],
    )?;
    build_support::set_record_field(
        &mut source,
        "new_with_shared",
        "GlobalState",
        "workspaces",
        "Arc::new(workspaces)",
    )?;
    build_support::set_record_field(
        &mut source,
        "snapshot",
        "GlobalStateSnapshot",
        "analysis",
        "self.shared.analysis()",
    )?;
    build_support::append_record_fields(
        &mut source,
        "snapshot",
        "GlobalStateSnapshot",
        &[build_support::FieldInit {
            name: "shared",
            value: Some("self.shared.clone()"),
        }],
    )?;
    build_support::rename::<ast::Fn>(&mut source, "target_spec_for_file", "_target_spec_for_file")?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_target_spec_for_file",
        "#[allow(dead_code)]",
    )?;
    build_support::extract(
        &mut source,
        "_target_spec_for_file",
        |function| {
            let workspace_loop = build_support::one(
                build_support::for_loops(function),
                "for loop in `_target_spec_for_file`",
            )?;
            build_support::through_tail(&workspace_loop, function)
        },
        build_support::Method {
            name: "target_spec_from_workspaces",
            receiver: Some("&self"),
            params: &[
                build_support::Param {
                    name: "path",
                    ty: "&paths::AbsPath",
                },
                build_support::Param {
                    name: "crate_id",
                    ty: "Crate",
                },
            ],
            args: &["path", "crate_id"],
            return_ty: Some("Option<TargetSpec>"),
        },
    )?;
    build_support::set_visibility::<ast::Fn>(
        &mut source,
        "target_spec_from_workspaces",
        "pub(crate)",
    )?;
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
        build_support::rename::<ast::Fn>(&mut source, name, &replacement)?;
        build_support::add_attr::<ast::Fn>(&mut source, &replacement, "#[allow(dead_code)]")?;
    }
    build_support::set_visibility::<ast::Fn>(&mut source, "enqueue_workspace_fetch", "pub(crate)")?;

    fs::write(global_state_rs, source)?;
    Ok(())
}

fn patch_main_loop_source(main_loop_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(main_loop_rs)?;
    build_support::add_use_alias(
        &mut source,
        Some("pub"),
        "crate::shared_analyzer::run_shared_rust_analyzer_lsp_session_with_config",
        "main_loop",
    )?;

    build_support::rename::<ast::Fn>(&mut source, "main_loop", "_main_loop")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_main_loop", "#[allow(dead_code)]")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "_main_loop", "pub(crate)")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "run", "pub(crate)")?;
    build_support::set_visibility::<ast::Enum>(&mut source, "Event", "pub(crate)")?;
    build_support::append::<ast::Enum>(
        &mut source,
        "Task",
        &[
            build_support::Variant {
                name: "FetchedWorkspace",
                tuple_fields: &["FetchWorkspaceResponse"],
            },
            build_support::Variant {
                name: "FetchedProcMacros",
                tuple_fields: &["crate::shared_analyzer::SharedProcMacroProgress"],
            },
            build_support::Variant {
                name: "SharedReloadReady",
                tuple_fields: &["crate::shared_analyzer::SharedAnalyzerOperationToken"],
            },
            build_support::Variant {
                name: "SharedRebuildReady",
                tuple_fields: &["crate::shared_analyzer::SharedAnalyzerOperationToken"],
            },
            build_support::Variant {
                name: "SharedBuildDataReady",
                tuple_fields: &[
                    "String",
                    "crate::shared_analyzer::SharedAnalyzerOperationToken",
                ],
            },
            build_support::Variant {
                name: "SharedProcMacrosReady",
                tuple_fields: &[
                    "String",
                    "crate::shared_analyzer::SharedAnalyzerOperationToken",
                ],
            },
            build_support::Variant {
                name: "WorkspaceUpdated",
                tuple_fields: &["crate::shared_analyzer::SharedAnalyzerRuntime"],
            },
            build_support::Variant {
                name: "RetryDeferred",
                tuple_fields: &["DeferredTask"],
            },
            build_support::Variant {
                name: "RetryDiscoverTests",
                tuple_fields: &["Vec<FileId>"],
            },
        ],
    )?;
    build_support::add_attr::<ast::Variant>(
        &mut source,
        "DiscoverProjectParam::Buildfile",
        "#[allow(dead_code)]",
    )?;

    for name in [
        "handle_event",
        "update_diagnostics",
        "update_tests",
        "handle_task",
    ] {
        let replacement = format!("_{name}");
        build_support::rename::<ast::Fn>(&mut source, name, &replacement)?;
    }

    build_support::extract(
        &mut source,
        "_update_diagnostics",
        |_| Ok(build_support::params_tail()),
        build_support::Method {
            name: "spawn_native_diagnostics",
            receiver: Some("&mut self"),
            params: &[
                build_support::Param {
                    name: "generation",
                    ty: "DiagnosticsGeneration",
                },
                build_support::Param {
                    name: "subscriptions",
                    ty: "std::sync::Arc<[FileId]>",
                },
            ],
            args: &["generation", "subscriptions"],
            return_ty: None,
        },
    )?;
    build_support::add_attr::<ast::Fn>(&mut source, "_update_diagnostics", "#[allow(dead_code)]")?;
    build_support::extract(
        &mut source,
        "_update_tests",
        |_| Ok(build_support::params_tail()),
        build_support::Method {
            name: "spawn_discover_tests",
            receiver: Some("&mut self"),
            params: &[build_support::Param {
                name: "subscriptions",
                ty: "Vec<FileId>",
            }],
            args: &["subscriptions"],
            return_ty: None,
        },
    )?;
    build_support::add_attr::<ast::Fn>(&mut source, "_update_tests", "#[allow(dead_code)]")?;
    build_support::redirect_call(
        &mut source,
        build_support::Scope::ForLoop {
            function: "spawn_native_diagnostics",
        },
        "snapshot",
        "pending_snapshot",
    )?;

    build_support::redirect_call(
        &mut source,
        build_support::Scope::MethodArgument {
            function: "spawn_discover_tests",
            method: "spawn",
        },
        "snapshot",
        "pending_snapshot",
    )?;
    build_support::delegate_closure(
        &mut source,
        build_support::ClosureDelegate {
            scope: build_support::Scope::Function("spawn_discover_tests"),
            call: build_support::Call::Method("spawn"),
            helper: "crate::main_loop::session::discover_tests",
            context: &[
                build_support::ClosureContext::Move("snapshot"),
                build_support::ClosureContext::Clone("subscriptions"),
            ],
            params: &[build_support::Param {
                name: "snapshot",
                ty: "_",
            }],
        },
    )?;

    build_support::redirect_call(
        &mut source,
        build_support::Scope::MethodArgument {
            function: "prime_caches",
            method: "spawn_with_sender",
        },
        "snapshot",
        "pending_snapshot",
    )?;
    build_support::delegate_closure(
        &mut source,
        build_support::ClosureDelegate {
            scope: build_support::Scope::Function("prime_caches"),
            call: build_support::Call::Method("spawn_with_sender"),
            helper: "crate::main_loop::session::prime_caches",
            context: &[build_support::ClosureContext::Move("analysis")],
            params: &[
                build_support::Param {
                    name: "analysis",
                    ty: "_",
                },
                build_support::Param {
                    name: "sender",
                    ty: "_",
                },
            ],
        },
    )?;

    for (scope, method, helper, context, params) in [
        (
            build_support::Scope::MatchArm {
                function: "handle_deferred_task",
                type_name: "DeferredTask",
                variant_name: "CheckIfIndexed",
            },
            "spawn_with_sender",
            "crate::main_loop::session::check_if_indexed",
            &[
                build_support::ClosureContext::Move("snap"),
                build_support::ClosureContext::Clone("uri"),
            ][..],
            &[
                build_support::Param {
                    name: "snap",
                    ty: "_",
                },
                build_support::Param {
                    name: "sender",
                    ty: "_",
                },
            ][..],
        ),
        (
            build_support::Scope::MatchArm {
                function: "handle_deferred_task",
                type_name: "DeferredTask",
                variant_name: "CheckProcMacroSources",
            },
            "spawn_with_sender",
            "crate::main_loop::session::check_proc_macro_sources",
            &[
                build_support::ClosureContext::Move("analysis"),
                build_support::ClosureContext::Clone("modified_rust_files"),
            ][..],
            &[
                build_support::Param {
                    name: "analysis",
                    ty: "_",
                },
                build_support::Param {
                    name: "sender",
                    ty: "_",
                },
            ][..],
        ),
    ] {
        build_support::redirect_call(&mut source, scope, "snapshot", "pending_snapshot")?;
        build_support::delegate_closure(
            &mut source,
            build_support::ClosureDelegate {
                scope,
                call: build_support::Call::Method(method),
                helper,
                context,
                params,
            },
        )?;
    }

    build_support::extract(
        &mut source,
        "_handle_event",
        |function| {
            let arm = build_support::one(
                build_support::arms(function, "PrimeCachesProgress", "End"),
                "`PrimeCachesProgress::End` arm",
            )?;
            let call = build_support::one(
                build_support::calls(&arm, "trigger_garbage_collection"),
                "`trigger_garbage_collection` call in the arm",
            )?;
            build_support::stmt(&call)
        },
        build_support::Method {
            name: "mark_prime_caches_gc",
            receiver: Some("&mut self"),
            params: &[],
            args: &[],
            return_ty: None,
        },
    )?;
    build_support::rename::<ast::Fn>(&mut source, "mark_prime_caches_gc", "_mark_prime_caches_gc")?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_mark_prime_caches_gc",
        "#[allow(dead_code)]",
    )?;

    build_support::extract(
        &mut source,
        "_handle_event",
        |function| {
            let idle = build_support::one(
                build_support::ifs_referencing(function, "last_gc_revision"),
                "idle gc guard",
            )?;
            let call = build_support::one(
                build_support::calls(&idle, "trigger_garbage_collection"),
                "`trigger_garbage_collection` call in the guard",
            )?;
            build_support::stmt(&call)
        },
        build_support::Method {
            name: "mark_idle_gc",
            receiver: Some("&mut self"),
            params: &[],
            args: &[],
            return_ty: None,
        },
    )?;
    build_support::rename::<ast::Fn>(&mut source, "mark_idle_gc", "_mark_idle_gc")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_mark_idle_gc", "#[allow(dead_code)]")?;

    build_support::extract(
        &mut source,
        "_handle_event",
        |function| {
            let guard = build_support::one(
                build_support::ifs_calling(function, "take_changes"),
                "diagnostics change guard",
            )?;
            let changes_loop =
                build_support::one(build_support::for_loops(&guard), "for loop in the guard")?;
            build_support::for_body(&changes_loop)
        },
        build_support::Method {
            name: "publish_changed_diagnostics",
            receiver: Some("&mut self"),
            params: &[build_support::Param {
                name: "file_id",
                ty: "FileId",
            }],
            args: &["file_id"],
            return_ty: None,
        },
    )?;
    build_support::rename::<ast::Fn>(
        &mut source,
        "publish_changed_diagnostics",
        "_publish_changed_diagnostics",
    )?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_publish_changed_diagnostics",
        "#[allow(dead_code)]",
    )?;

    build_support::extract(
        &mut source,
        "handle_flycheck_msg",
        |function| {
            let arm = build_support::one(
                build_support::arms(function, "FlycheckMessage", "AddDiagnostic"),
                "`FlycheckMessage::AddDiagnostic` arm",
            )?;
            let diagnostics_loop =
                build_support::one(build_support::for_loops(&arm), "for loop in the arm")?;
            build_support::for_body(&diagnostics_loop)
        },
        build_support::Method {
            name: "record_flycheck_diagnostic",
            receiver: Some("&mut self"),
            params: &[
                build_support::Param {
                    name: "id",
                    ty: "usize",
                },
                build_support::Param {
                    name: "generation",
                    ty: "DiagnosticsGeneration",
                },
                build_support::Param {
                    name: "package_id",
                    ty: "Option<crate::flycheck::PackageSpecifier>",
                },
                build_support::Param {
                    name: "diag",
                    ty: "crate::diagnostics::flycheck_to_proto::MappedRustDiagnostic",
                },
            ],
            args: &["id", "generation", "package_id.clone()", "diag"],
            return_ty: None,
        },
    )?;
    build_support::rename::<ast::Fn>(
        &mut source,
        "record_flycheck_diagnostic",
        "_record_flycheck_diagnostic",
    )?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_record_flycheck_diagnostic",
        "#[allow(dead_code)]",
    )?;

    build_support::append_record_fields(
        &mut source,
        "_handle_task",
        "FetchWorkspaceResponse",
        &[
            build_support::FieldInit {
                name: "shared",
                value: Some("self.shared.clone()"),
            },
            build_support::FieldInit {
                name: "reload_id",
                value: Some("None"),
            },
            build_support::FieldInit {
                name: "adopted",
                value: Some("false"),
            },
        ],
    )?;
    build_support::append_record_fields(
        &mut source,
        "_handle_task",
        "FetchBuildDataResponse",
        &[
            build_support::FieldInit {
                name: "rebuild_id",
                value: Some("self.build_data_rebuild_id"),
            },
            build_support::FieldInit {
                name: "reload",
                value: Some("self.build_data_reload"),
            },
        ],
    )?;

    build_support::rename_path_root(&mut source, "_handle_task", "Task", "UpstreamTask")?;
    build_support::add_use(&mut source, None, "self::session::UpstreamTask")?;

    let session = owned_source_path("session.rs");
    build_support::mount_module(&mut source, Some("pub(crate)"), "session", &session)?;

    fs::write(main_loop_rs, source)?;
    Ok(())
}

fn patch_session_source(session_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(session_rs)?;
    build_support::append::<ast::Enum>(
        &mut source,
        "IoThreads",
        &[build_support::Variant {
            name: "External",
            tuple_fields: &[],
        }],
    )?;
    build_support::append_match_arms(
        &mut source,
        build_support::Scope::MatchArm {
            function: "join",
            type_name: "IoThreads",
            variant_name: "Stdio",
        },
        &[build_support::MatchArm {
            pattern: "IoThreads::External",
            expression: "Ok(())",
        }],
    )?;

    fs::write(session_rs, source)?;
    Ok(())
}

fn patch_op_queue_source(op_queue_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(op_queue_rs)?;
    build_support::set_visibility::<ast::RecordField>(&mut source, "last_op_result", "pub(crate)")?;
    fs::write(op_queue_rs, source)?;
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
        build_support::rename::<ast::Fn>(&mut source, name, &replacement)?;
    }
    for name in [
        "fetch_workspaces",
        "fetch_proc_macros",
        "recreate_crate_graph",
    ] {
        let replacement = format!("_{name}");
        build_support::add_attr::<ast::Fn>(&mut source, &replacement, "#[allow(dead_code)]")?;
    }

    build_support::set_visibility::<ast::Fn>(&mut source, "reload_flycheck", "pub(crate)")?;

    build_support::add_rest_pattern(&mut source, "switch_workspaces", "FetchWorkspaceResponse")?;
    build_support::add_rest_pattern(&mut source, "switch_workspaces", "FetchBuildDataResponse")?;
    build_support::redirect_call(
        &mut source,
        build_support::Scope::Function("switch_workspaces"),
        "recreate_crate_graph",
        "recreate_crate_graph_from_shared",
    )?;
    build_support::rename::<ast::Fn>(&mut source, "switch_workspaces", "_switch_workspaces")?;
    build_support::extract(
        &mut source,
        "_switch_workspaces",
        |function| {
            let branch = build_support::one(
                build_support::ifs_calling(function, "expand_proc_macros"),
                "proc-macro client branch",
            )?;
            build_support::stmt(&branch)
        },
        build_support::Method {
            name: "set_proc_macro_clients",
            receiver: Some("&mut self"),
            params: &[build_support::Param {
                name: "same_workspaces",
                ty: "bool",
            }],
            args: &["same_workspaces"],
            return_ty: None,
        },
    )?;
    build_support::rename::<ast::Fn>(
        &mut source,
        "set_proc_macro_clients",
        "_set_proc_macro_clients",
    )?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_set_proc_macro_clients",
        "#[allow(dead_code)]",
    )?;

    fs::write(reload_rs, source)?;
    Ok(())
}

fn patch_dispatch_source(dispatch_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(dispatch_rs)?;

    let shared_dispatch = owned_source_path("shared_dispatch.rs");
    build_support::mount_module(&mut source, None, "shared_dispatch", &shared_dispatch)?;
    println!("cargo:rerun-if-changed={}", shared_dispatch.display());
    build_support::redirect_call(
        &mut source,
        build_support::Scope::Function("on_with_thread_intent"),
        "snapshot",
        "pending_snapshot",
    )?;
    build_support::delegate_closure(
        &mut source,
        build_support::ClosureDelegate {
            scope: build_support::Scope::Function("on_with_thread_intent"),
            call: build_support::Call::Method("spawn"),
            helper: "crate::handlers::dispatch::shared_dispatch::on_with_thread_intent",
            context: &[
                build_support::ClosureContext::Move("world"),
                build_support::ClosureContext::Clone("req"),
            ],
            params: &[build_support::Param {
                name: "world",
                ty: "_",
            }],
        },
    )?;

    fs::write(dispatch_rs, source)?;
    Ok(())
}

fn patch_request_source(request_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(request_rs)?;

    for name in ["handle_workspace_reload", "handle_proc_macros_rebuild"] {
        let replacement = format!("_{name}");
        build_support::rename::<ast::Fn>(&mut source, name, &replacement)?;
        build_support::add_attr::<ast::Fn>(&mut source, &replacement, "#[allow(dead_code)]")?;
        build_support::add_use(
            &mut source,
            Some("pub(crate)"),
            &format!("crate::shared_reload::{name}"),
        )?;
    }

    fs::write(request_rs, source)?;
    Ok(())
}

fn patch_flycheck_to_proto_source(flycheck_to_proto_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(flycheck_to_proto_rs)?;

    build_support::rename::<ast::Fn>(&mut source, "location", "_location")?;
    build_support::add_use(&mut source, None, "self::flycheck_location::location")?;
    let flycheck_location = owned_source_path("diagnostics/flycheck_location.rs");
    build_support::mount_module(&mut source, None, "flycheck_location", &flycheck_location)?;
    println!("cargo:rerun-if-changed={}", flycheck_location.display());
    fs::write(flycheck_to_proto_rs, source)?;
    Ok(())
}

fn patch_notification_source(notification_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(notification_rs)?;

    build_support::redirect_call(
        &mut source,
        build_support::Scope::IfLet {
            function: "run_flycheck",
            type_name: "FileExcluded",
            variant_name: "No",
        },
        "snapshot",
        "pending_snapshot",
    )?;
    let once = build_support::Scope::MatchArm {
        function: "run_flycheck",
        type_name: "InvocationStrategy",
        variant_name: "Once",
    };
    build_support::extract_match_arm(
        &mut source,
        once,
        build_support::Function {
            name: "run_flycheck_once",
            params: &[
                build_support::Param {
                    name: "world",
                    ty: "crate::shared_global_state::PendingGlobalStateSnapshot",
                },
                build_support::Param {
                    name: "vfs_path",
                    ty: "vfs::VfsPath",
                },
            ],
            args: &["world", "vfs_path.clone()"],
            return_ty: Some("Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe>"),
        },
    )?;
    build_support::delegate_closure(
        &mut source,
        build_support::ClosureDelegate {
            scope: build_support::Scope::Function("run_flycheck_once"),
            call: build_support::Call::Function("Box::new"),
            helper: "crate::shared_notification::activate_flycheck",
            context: &[build_support::ClosureContext::Move("world")],
            params: &[build_support::Param {
                name: "world",
                ty: "_",
            }],
        },
    )?;
    let per_workspace = build_support::Scope::MatchArm {
        function: "run_flycheck",
        type_name: "InvocationStrategy",
        variant_name: "PerWorkspace",
    };
    build_support::extract_match_arm(
        &mut source,
        per_workspace,
        build_support::Function {
            name: "run_flycheck_per_workspace",
            params: &[
                build_support::Param {
                    name: "world",
                    ty: "crate::shared_global_state::PendingGlobalStateSnapshot",
                },
                build_support::Param {
                    name: "file_id",
                    ty: "ide::FileId",
                },
                build_support::Param {
                    name: "vfs_path",
                    ty: "vfs::VfsPath",
                },
                build_support::Param {
                    name: "may_flycheck_workspace",
                    ty: "bool",
                },
            ],
            args: &[
                "world",
                "file_id",
                "vfs_path.clone()",
                "may_flycheck_workspace",
            ],
            return_ty: Some("Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe>"),
        },
    )?;
    build_support::delegate_closure(
        &mut source,
        build_support::ClosureDelegate {
            scope: build_support::Scope::Function("run_flycheck_per_workspace"),
            call: build_support::Call::Function("Box::new"),
            helper: "crate::shared_notification::select_flycheck_per_workspace",
            context: &[build_support::ClosureContext::Move("world")],
            params: &[build_support::Param {
                name: "world",
                ty: "_",
            }],
        },
    )?;
    for name in [
        "run_flycheck",
        "run_flycheck_once",
        "run_flycheck_per_workspace",
    ] {
        build_support::set_visibility::<ast::Fn>(&mut source, name, "pub(crate)")?;
    }
    build_support::rename::<ast::Fn>(&mut source, "run_flycheck", "_run_flycheck")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_run_flycheck", "#[allow(dead_code)]")?;
    build_support::add_use(
        &mut source,
        Some("pub(crate)"),
        "crate::shared_notification::run_flycheck",
    )?;
    build_support::rename::<ast::Fn>(
        &mut source,
        "handle_did_save_text_document",
        "_handle_did_save_text_document",
    )?;
    build_support::add_attr::<ast::Fn>(
        &mut source,
        "_handle_did_save_text_document",
        "#[allow(dead_code)]",
    )?;
    build_support::add_use(
        &mut source,
        Some("pub(crate)"),
        "crate::shared_notification::handle_did_save_text_document",
    )?;

    fs::write(notification_rs, source)?;
    Ok(())
}

fn patch_task_pool_source(task_pool_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(task_pool_rs)?;
    build_support::set_visibility::<ast::RecordField>(
        &mut source,
        "TaskPool::sender",
        "pub(crate)",
    )?;
    fs::write(task_pool_rs, source)?;
    Ok(())
}

fn patch_driver_source(main_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(main_rs)?;

    build_support::set_visibility::<ast::Fn>(&mut source, "main", "pub")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "setup_logging", "pub")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "wait_for_debugger", "pub")?;
    build_support::add_use_alias(&mut source, None, "crate", "rust_analyzer")?;

    fs::write(main_rs, source)?;
    Ok(())
}

fn patch_slow_tests(slow_tests: &Path) -> Result<(), Box<dyn Error>> {
    for name in ["main.rs", "ratoml.rs", "cli.rs", "flycheck.rs"] {
        let path = slow_tests.join(name);
        let mut source = fs::read_to_string(&path)?;
        build_support::retarget_use(
            &mut source,
            "skip_slow_tests",
            "crate::test_support::skip_slow_tests",
        )?;
        fs::write(path, source)?;
    }

    let support = slow_tests.join("support.rs");
    let mut source = fs::read_to_string(&support)?;
    build_support::rename::<ast::Fn>(&mut source, "lines_match", "_original_lines_match")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "_original_lines_match", "pub(crate)")?;
    build_support::add_use(&mut source, None, "crate::test_support::lines_match")?;
    fs::write(support, source)?;

    let ratoml = slow_tests.join("ratoml.rs");
    let mut source = fs::read_to_string(&ratoml)?;
    build_support::rename::<ast::Fn>(&mut source, "fixture_path", "_original_fixture_path")?;
    build_support::mount_module(
        &mut source,
        None,
        "fixture_uri",
        &owned_source_path("slow_tests_uri.rs"),
    )?;
    build_support::add_use(&mut source, None, "self::fixture_uri::FixturePath")?;
    fs::write(ratoml, source)?;
    Ok(())
}

fn write_slow_tests_wrapper(slow_tests: &Path) -> Result<(), Box<dyn Error>> {
    let test_support = owned_source_path("slow_tests.rs");
    let main_rs = slow_tests.join("main.rs");
    let wrapper_rs = slow_tests.join("test-support.rs");
    fs::write(
        &wrapper_rs,
        format!(
            "extern crate ra_ap_rust_analyzer as rust_analyzer;\n#[path = {:?}]\nmod test_support;\ninclude!({:?});\n",
            test_support.to_string_lossy().into_owned(),
            main_rs.to_string_lossy().into_owned(),
        ),
    )?;
    println!("cargo:rerun-if-changed={}", test_support.display());
    Ok(())
}
