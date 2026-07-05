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
    let (generated, package) =
        build_support::prepare_bridge_package(RA_PACKAGE, "ra_ap_rust_analyzer_bridge")?;
    let revision = package
        .git_revision
        .as_deref()
        .ok_or("ra_ap_rust-analyzer does not contain .cargo_vcs_info.json")?;
    let pinned = pinned_upstream_release()?;
    let release = if offline_build() {
        pinned
    } else {
        match rust_analyzer_release(revision) {
            Ok(release) => {
                if release != pinned {
                    return Err(format!(
                        "[package.metadata.upstream] release is {pinned}, but the rust-analyzer \
                         release for commit {revision} is {release}"
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
    patch_global_state_source(&generated_src.join("global_state.rs"))?;
    patch_main_loop_source(&generated_src.join("main_loop.rs"))?;
    patch_reload_source(&generated_src.join("reload.rs"))?;
    patch_flycheck_to_proto_source(&generated_src.join("diagnostics/flycheck_to_proto.rs"))?;
    patch_notification_source(&generated_src.join("handlers/notification.rs"))?;
    patch_driver_source(&generated_src.join("bin/main.rs"))?;
    patch_test_tool_attributes(&generated_src)?;
    write_root_module(
        &generated_src.join("root.rs"),
        &generated_src.join("lib.rs"),
    )?;
    let slow_tests = generated.join("tests/slow-tests");
    patch_slow_tests(&slow_tests)?;
    let slow_tests_wrapper = write_slow_tests_wrapper(&slow_tests)?;
    println!(
        "cargo:rustc-env=ANALYZED_RA_CRATE_VERSION={}",
        package.version
    );
    println!("cargo:rustc-env=ANALYZED_RA_RELEASE_VERSION={}", release);
    println!("cargo:rustc-env=ANALYZED_RA_COMMIT_HASH={revision}");
    println!(
        "cargo:rustc-env=ANALYZED_RA_VERSION={} {}",
        release,
        &revision[..8]
    );
    println!(
        "cargo:rustc-env=ANALYZED_RA_SLOW_TESTS={}",
        slow_tests_wrapper.display()
    );
    println!("cargo:rerun-if-env-changed=GITHUB_TOKEN");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn pinned_upstream_release() -> Result<String, Box<dyn Error>> {
    let manifest_path = Path::new(&env::var("CARGO_MANIFEST_DIR")?).join("Cargo.toml");
    let manifest: toml::Table = toml::from_str(&fs::read_to_string(manifest_path)?)?;
    manifest
        .get("package")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("upstream"))
        .and_then(|value| value.get("release"))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| "Cargo.toml lacks a [package.metadata.upstream] release".into())
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

fn rust_analyzer_release(revision: &str) -> Result<String, Box<dyn Error>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();

    let tag = rust_analyzer_release_tag(&agent, revision)?;
    let release = github_get(
        &agent,
        &format!("/repos/{RA_REPOSITORY}/releases/tags/{tag}"),
    )?;
    let body = release
        .get("body")
        .and_then(serde_json::Value::as_str)
        .ok_or("rust-analyzer release has no body")?;
    if release_commit(body) != Some(revision) {
        return Err(
            format!("rust-analyzer release {tag} does not describe commit {revision}").into(),
        );
    }
    let version = release_version(body).ok_or("rust-analyzer release has no extension version")?;

    Ok(version.to_owned())
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

fn write_root_module(root_rs: &Path, lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let shared_analyzer = owned_source_path("shared_analyzer.rs");
    let shared_global_state = owned_source_path("global_state.rs");
    let shared_main_loop = owned_source_path("main_loop.rs");
    let shared_reload = owned_source_path("reload.rs");
    let shared_notification = owned_source_path("handlers/notification.rs");
    let mut upstream_root = fs::read_to_string(lib_rs)?;
    let handlers_start = "mod handlers {\n";
    let insert_at = upstream_root
        .find(handlers_start)
        .map(|index| index + handlers_start.len())
        .ok_or("could not find handlers module")?;
    upstream_root.insert_str(
        insert_at,
        &format!(
            "    #[path = {:?}]\n    pub(crate) mod shared_notification;\n",
            shared_notification.to_string_lossy().into_owned()
        ),
    );
    let source = format!(
        r#"
#[path = {:?}]
pub mod shared_analyzer;

#[path = {:?}]
pub(crate) mod shared_global_state;

#[path = {:?}]
pub(crate) mod shared_main_loop;

#[path = {:?}]
pub(crate) mod shared_reload;

{upstream_root}

#[path = {:?}]
pub mod driver;

pub use shared_analyzer::{{
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
        shared_analyzer.to_string_lossy().into_owned(),
        shared_global_state.to_string_lossy().into_owned(),
        shared_main_loop.to_string_lossy().into_owned(),
        shared_reload.to_string_lossy().into_owned(),
        lib_rs
            .with_file_name("bin/main.rs")
            .to_string_lossy()
            .into_owned()
    );
    fs::write(root_rs, source)?;
    println!("cargo:rerun-if-changed={}", shared_analyzer.display());
    println!("cargo:rerun-if-changed={}", shared_global_state.display());
    println!("cargo:rerun-if-changed={}", shared_main_loop.display());
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
    source = source.replace("ra_ap_rust_analyzer", "rust_analyzer");

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

fn patch_global_state_source(global_state_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(global_state_rs)?;

    build_support::append::<ast::Struct>(
        &mut source,
        "FetchWorkspaceResponse",
        &[build_support::Field {
            vis: Some("pub(crate)"),
            name: "shared",
            ty: "crate::shared_analyzer::SharedAnalyzerRuntime",
        }],
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
                name: "provider",
                ty: "crate::shared_analyzer::SharedAnalyzerProvider",
            },
            build_support::Field {
                vis: Some("pub(crate)"),
                name: "shared",
                ty: "crate::shared_analyzer::SharedAnalyzerRuntime",
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
    build_support::set_visibility::<ast::RecordField>(
        &mut source,
        "GlobalStateSnapshot::mem_docs",
        "pub(crate)",
    )?;
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
                name: "provider",
                ty: "crate::shared_analyzer::SharedAnalyzerProvider",
            },
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
                name: "provider",
                value: None,
            },
            build_support::FieldInit {
                name: "shared",
                value: None,
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
    build_support::add_use(
        &mut source,
        Some("pub"),
        "crate::shared_main_loop::main_loop",
    )?;

    build_support::rename::<ast::Fn>(&mut source, "main_loop", "_main_loop")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_main_loop", "#[allow(dead_code)]")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "_main_loop", "pub(crate)")?;
    build_support::add_attr::<ast::Fn>(&mut source, "run", "#[allow(dead_code)]")?;
    build_support::set_visibility::<ast::Enum>(&mut source, "Event", "pub(crate)")?;
    build_support::append::<ast::Enum>(
        &mut source,
        "Task",
        &[build_support::Variant {
            name: "FetchedWorkspace",
            tuple_fields: &["FetchWorkspaceResponse"],
        }],
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

    build_support::extract(
        &mut source,
        "_handle_event",
        |function| {
            let arm = build_support::one(
                build_support::arms(function, "PrimeCachesProgress::End"),
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
            name: "mark_gc_when_idle",
            receiver: Some("&mut self"),
            params: &[],
            args: &[],
            return_ty: None,
        },
    )?;
    build_support::rename::<ast::Fn>(&mut source, "mark_gc_when_idle", "_mark_gc_when_idle")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_mark_gc_when_idle", "#[allow(dead_code)]")?;

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
                build_support::arms(function, "FlycheckMessage::AddDiagnostic"),
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
        &[build_support::FieldInit {
            name: "shared",
            value: Some("self.shared.clone()"),
        }],
    )?;
    build_support::rename_path_root(&mut source, "_handle_task", "Task", "UpstreamTask")?;
    build_support::add_use(&mut source, None, "self::session::UpstreamTask")?;

    let session = owned_source_path("session.rs");
    source.push_str(&format!(
        "\n#[path = {:?}]\npub(crate) mod session;\n",
        session.to_string_lossy().into_owned()
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
        build_support::rename::<ast::Fn>(&mut source, name, &replacement)?;
        build_support::add_attr::<ast::Fn>(&mut source, &replacement, "#[allow(dead_code)]")?;
    }

    build_support::set_visibility::<ast::Fn>(&mut source, "reload_flycheck", "pub(crate)")?;

    build_support::add_rest_pattern(&mut source, "switch_workspaces", "FetchWorkspaceResponse")?;
    build_support::redirect_call(
        &mut source,
        "switch_workspaces",
        "recreate_crate_graph",
        "recreate_crate_graph_from_shared",
    )?;

    fs::write(reload_rs, source)?;
    Ok(())
}

fn patch_flycheck_to_proto_source(flycheck_to_proto_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(flycheck_to_proto_rs)?;

    build_support::rename::<ast::Fn>(&mut source, "location", "_location")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_location", "#[allow(dead_code)]")?;
    build_support::add_use(&mut source, None, "self::flycheck_location::location")?;
    let flycheck_location = owned_source_path("diagnostics/flycheck_location.rs");
    source.push_str(&format!(
        "\n#[path = {:?}]\nmod flycheck_location;\n",
        flycheck_location.to_string_lossy().into_owned()
    ));
    println!("cargo:rerun-if-changed={}", flycheck_location.display());
    fs::write(flycheck_to_proto_rs, source)?;
    Ok(())
}

fn patch_notification_source(notification_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(notification_rs)?;

    build_support::rename::<ast::Fn>(&mut source, "run_flycheck", "_run_flycheck")?;
    build_support::add_attr::<ast::Fn>(&mut source, "_run_flycheck", "#[allow(dead_code)]")?;
    build_support::add_use(
        &mut source,
        Some("pub(crate)"),
        "crate::handlers::shared_notification::run_flycheck",
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
        "crate::handlers::shared_notification::handle_did_save_text_document",
    )?;

    fs::write(notification_rs, source)?;
    Ok(())
}

fn patch_driver_source(main_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(main_rs)?;

    build_support::set_visibility::<ast::Fn>(&mut source, "main", "pub")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "setup_logging", "pub")?;
    build_support::set_visibility::<ast::Fn>(&mut source, "wait_for_debugger", "pub")?;

    fs::write(main_rs, source)?;
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
            "use crate::test_support::skip_slow_tests;\n",
        )
        .replace(
            r#".replace("C:\\", "/c:/").replace('\\', "/")"#,
            ".uri_path()",
        );
    if source.contains(".uri_path()") {
        build_support::add_use(&mut source, None, "crate::test_support::UriPath")?;
    }
    fs::write(path, source)?;
    Ok(())
}

fn write_slow_tests_wrapper(slow_tests: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let test_support = owned_source_path("slow_tests.rs");
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

    let body_rs = slow_tests.join("test-support-main.rs");
    fs::write(&body_rs, body)?;
    let wrapper_rs = slow_tests.join("test-support.rs");
    fs::write(
        &wrapper_rs,
        format!(
            "#[path = {:?}]\nmod test_support;\n#[path = {:?}]\nmod cli;\n#[path = {:?}]\nmod flycheck;\n#[path = {:?}]\nmod ratoml;\n#[path = {:?}]\nmod support;\n#[path = {:?}]\nmod testdir;\ninclude!({:?});\n",
            test_support.to_string_lossy().into_owned(),
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
    println!("cargo:rerun-if-changed={}", test_support.display());
    Ok(wrapper_rs)
}
