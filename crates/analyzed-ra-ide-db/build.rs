use analyzed_bridge as build_support;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const PACKAGE: &str = "ra_ap_ide_db";
const GENERATED_DIR: &str = "ra_ap_ide_db_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_ide_db_source(&generated.join("src/lib.rs"))?;
    patch_search_source(&generated.join("src/search.rs"))?;
    patch_symbol_index_source(&generated.join("src/symbol_index.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_db_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    let analyzed = owned_source_path("analyzed.rs");
    build_support::prepend_path_module(&mut source, None, "analyzed", &analyzed);
    println!("cargo:rerun-if-changed={}", analyzed.display());
    build_support::append_struct_fields(
        &mut source,
        "RootDatabase",
        "    analyzed_visible_files: Option<std::sync::Arc<rustc_hash::FxHashSet<vfs::FileId>>>,\n",
    )?;
    build_support::append_record_expr_fields_in_function(
        &mut source,
        "clone",
        "Self",
        "            analyzed_visible_files: self.analyzed_visible_files.clone(),\n",
    )?;
    build_support::append_record_expr_fields_in_function(
        &mut source,
        "new",
        "RootDatabase",
        "            analyzed_visible_files: None,\n",
    )?;
    fs::write(lib_rs, source)?;
    Ok(())
}

fn patch_search_source(search_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(search_rs)?;

    build_support::retarget_use_tree(
        &mut source,
        "all_crates",
        "crate::analyzed::visible_base_crates",
        "all_crates",
    )?;
    let analyzed_search_scope = owned_source_path("search_scope.rs");
    source.push_str(&format!(
        "\n#[path = {:?}]\nmod analyzed_search_scope;\n",
        analyzed_search_scope.to_string_lossy().into_owned()
    ));
    println!("cargo:rerun-if-changed={}", analyzed_search_scope.display());
    build_support::rename_function(&mut source, "reverse_dependencies", "_reverse_dependencies")?;
    build_support::allow_dead_code_for_function(&mut source, "_reverse_dependencies")?;

    fs::write(search_rs, source)?;
    Ok(())
}

fn patch_symbol_index_source(symbol_index_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(symbol_index_rs)?;

    let analyzed_symbol_index = owned_source_path("symbol_index.rs");
    source.push_str(&format!(
        "\n#[path = {:?}]\nmod analyzed_symbol_index;\npub use analyzed_symbol_index::world_symbols;\n",
        analyzed_symbol_index.to_string_lossy().into_owned()
    ));
    println!("cargo:rerun-if-changed={}", analyzed_symbol_index.display());
    build_support::rename_function(&mut source, "world_symbols", "_world_symbols")?;
    build_support::allow_dead_code_for_function(&mut source, "_world_symbols")?;
    build_support::rename_function(
        &mut source,
        "resolve_path_to_modules",
        "_resolve_path_to_modules",
    )?;
    build_support::allow_dead_code_for_function(&mut source, "_resolve_path_to_modules")?;
    build_support::inject_use(&mut source, "crate::analyzed::resolve_path_to_modules")?;

    fs::write(symbol_index_rs, source)?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
