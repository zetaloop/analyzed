use analyzed_bridge as build_support;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use analyzed_bridge::replace_once;

const PACKAGE: &str = "ra_ap_ide";
const GENERATED_DIR: &str = "ra_ap_ide_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_ide_source(&generated.join("src/lib.rs"))?;
    patch_view_crate_graph_source(&generated.join("src/view_crate_graph.rs"))?;
    patch_skip_slow_tests(&generated.join("src/syntax_highlighting/tests.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    let analyzed = owned_source_path("analyzed.rs");
    replace_once(
        &mut source,
        "mod annotations;\n",
        &format!(
            "#[path = {:?}]\nmod analyzed;\n\nmod annotations;\n",
            analyzed.to_string_lossy().into_owned()
        ),
    )?;
    println!("cargo:rerun-if-changed={}", analyzed.display());

    fs::write(lib_rs, source)?;
    Ok(())
}

fn patch_view_crate_graph_source(view_crate_graph_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(view_crate_graph_rs)?;

    replace_once(
        &mut source,
        "    let all_crates = all_crates(db);\n    let crates_to_render = all_crates\n        .iter()\n        .copied()\n",
        "    let all_crates = db.analyzed_visible_base_crates(all_crates(db).iter().copied());\n    let crates_to_render = all_crates\n        .into_iter()\n",
    )?;

    fs::write(view_crate_graph_rs, source)?;
    Ok(())
}

// The upstream skip_slow_tests helper writes a cookie into the rust-analyzer
// checkout when slow tests run, which resolves to the cargo registry source
// cache for registry packages. The benchmark tests load bench_data from the
// checkout, which the registry package does not contain.
fn patch_skip_slow_tests(tests_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(tests_rs)?;

    replace_once(
        &mut source,
        "use test_utils::{AssertLinear, bench, bench_fixture, skip_slow_tests};\n",
        "use crate::analyzed::skip_slow_tests;\nuse test_utils::{AssertLinear, bench, bench_fixture};\n",
    )?;
    for benchmark in [
        "benchmark_syntax_highlighting_long_struct",
        "syntax_highlighting_not_quadratic",
        "benchmark_syntax_highlighting_parser",
    ] {
        replace_once(
            &mut source,
            &format!("#[test]\nfn {benchmark}() {{\n"),
            &format!(
                "#[test]\n#[ignore = \"bench_data not available in registry packages\"]\nfn {benchmark}() {{\n"
            ),
        )?;
    }

    fs::write(tests_rs, source)?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
