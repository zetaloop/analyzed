use analyzed_bridge as build_support;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const PACKAGE: &str = "ra_ap_ide";
const GENERATED_DIR: &str = "ra_ap_ide_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_ide_source(&generated.join("src/lib.rs"))?;
    patch_view_crate_graph_source(&generated.join("src/view_crate_graph.rs"))?;
    patch_syntax_highlighting_benches(&generated.join("src/syntax_highlighting/tests.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    let analyzed = owned_source_path("analyzed.rs");
    build_support::prepend_path_module(&mut source, None, "analyzed", &analyzed);
    println!("cargo:rerun-if-changed={}", analyzed.display());

    fs::write(lib_rs, source)?;
    Ok(())
}

fn patch_view_crate_graph_source(view_crate_graph_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(view_crate_graph_rs)?;

    build_support::retarget_use_tree(
        &mut source,
        "all_crates",
        "crate::analyzed::visible_crates_for_graph",
        "all_crates",
    )?;

    fs::write(view_crate_graph_rs, source)?;
    Ok(())
}

// The upstream skip_slow_tests helper writes a cookie into the rust-analyzer
// checkout when slow tests run, which resolves to the cargo registry source
// cache for registry packages. The benchmark tests load bench_data from the
// checkout, which the registry package does not contain.
fn patch_syntax_highlighting_benches(tests_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(tests_rs)?;

    for benchmark in [
        "benchmark_syntax_highlighting_long_struct",
        "syntax_highlighting_not_quadratic",
        "benchmark_syntax_highlighting_parser",
    ] {
        build_support::add_function_attribute(
            &mut source,
            benchmark,
            "#[ignore = \"bench_data not available in registry packages\"]",
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
