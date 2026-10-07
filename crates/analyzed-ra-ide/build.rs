use analyzed_bridge as build_support;
use r#override::{Source, item, root};

use std::{env, error::Error, path::PathBuf};

const PACKAGE: &str = "ra_ap_ide";
const GENERATED_DIR: &str = "ra_ap_ide_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut sources, _) = build_support::prepare_bridge_package(
        PACKAGE,
        GENERATED_DIR,
        &[],
        &[
            "ide-assists",
            "ide-completion",
            "ide-db",
            "ide-diagnostics",
            "ide-ssr",
        ],
    )?;
    sources.edit("src/lib.rs", patch_ide_source)?;
    sources.edit("src/view_crate_graph.rs", patch_view_crate_graph_source)?;
    sources.edit(
        "src/syntax_highlighting/tests.rs",
        patch_syntax_highlighting_benches,
    )?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(root())?
        .mount_module("mod visibility", owned_source_path("visibility.rs"))?;
    source
        .select(item("Analysis"))?
        .add_field("guard: Option<crate::visibility::AnalysisGuard>")?;
    source
        .select(item("AnalysisHost::analysis").record("Analysis"))?
        .add_field("guard: None")?;
    source
        .select(item("Analysis::from_ra_fixture_with_on_cursor").record("Analysis"))?
        .add_field("guard: None")?;

    Ok(())
}

fn patch_view_crate_graph_source(source: &mut Source) -> Result<(), Box<dyn Error>> {
    source
        .select(root().import("all_crates"))?
        .redirect("crate::visibility::all_crates")?;
    Ok(())
}

// The upstream skip_slow_tests helper writes a cookie into the rust-analyzer
// checkout when slow tests run, which resolves to the cargo registry source
// cache for registry packages. The benchmark tests load bench_data from the
// checkout, which the registry package does not contain.
fn patch_syntax_highlighting_benches(source: &mut Source) -> Result<(), Box<dyn Error>> {
    for benchmark in [
        "benchmark_syntax_highlighting_long_struct",
        "syntax_highlighting_not_quadratic",
        "benchmark_syntax_highlighting_parser",
    ] {
        source
            .select(item(benchmark))?
            .add_attribute("#[ignore = \"bench_data not available in registry packages\"]")?;
    }
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
