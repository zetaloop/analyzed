use analyzed_bridge as build_support;

use std::{error::Error, fs, path::Path};

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

    replace_once(
        &mut source,
        "    /// Returns a snapshot of the current state, which you can query for\n    /// semantic information.\n    pub fn analysis(&self) -> Analysis {\n        Analysis { db: self.db.clone() }\n    }\n",
        "    /// Returns a snapshot of the current state, which you can query for\n    /// semantic information.\n    pub fn analysis(&self) -> Analysis {\n        Analysis { db: self.db.clone() }\n    }\n\n    pub fn analyzed_analysis_with_visible_files(\n        &self,\n        visible_files: std::sync::Arc<rustc_hash::FxHashSet<FileId>>,\n    ) -> Analysis {\n        Analysis { db: self.db.clone().analyzed_with_visible_files(visible_files) }\n    }\n",
    )?;

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
        "use test_utils::{AssertLinear, bench, bench_fixture};\n\n#[allow(dead_code)]\nfn skip_slow_tests() -> bool {\n    (std::env::var(\"CI\").is_err() && std::env::var(\"RUN_SLOW_TESTS\").is_err())\n        || std::env::var(\"SKIP_SLOW_TESTS\").is_ok()\n}\n",
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
