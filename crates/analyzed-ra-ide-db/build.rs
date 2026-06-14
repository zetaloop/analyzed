use analyzed_bridge as build_support;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use analyzed_bridge::replace_once;

const PACKAGE: &str = "ra_ap_ide_db";
const GENERATED_DIR: &str = "ra_ap_ide_db_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_ide_db_source(&generated.join("src/lib.rs"))?;
    patch_search_source(&generated.join("src/search.rs"))?;
    patch_symbol_index_source(&generated.join("src/symbol_index.rs"))?;
    patch_node_ext_source(&generated.join("src/syntax_helpers/node_ext.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_db_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    let analyzed = owned_source_path("analyzed.rs");
    replace_once(
        &mut source,
        "pub use span::{self, FileId};\n",
        &format!(
            "pub use span::{{self, FileId}};\n\n#[path = {:?}]\nmod analyzed;\n",
            analyzed.to_string_lossy().into_owned()
        ),
    )?;
    println!("cargo:rerun-if-changed={}", analyzed.display());
    replace_once(
        &mut source,
        "    crates_map: Arc<CratesMap>,\n    nonce: Nonce,\n",
        "    crates_map: Arc<CratesMap>,\n    analyzed_visible_files: Option<std::sync::Arc<rustc_hash::FxHashSet<vfs::FileId>>>,\n    nonce: Nonce,\n",
    )?;
    replace_once(
        &mut source,
        "            crates_map: self.crates_map.clone(),\n            nonce: self.nonce,\n",
        "            crates_map: self.crates_map.clone(),\n            analyzed_visible_files: self.analyzed_visible_files.clone(),\n            nonce: self.nonce,\n",
    )?;
    replace_once(
        &mut source,
        "            files: Default::default(),\n            crates_map: Default::default(),\n            nonce: Nonce::new(),\n",
        "            files: Default::default(),\n            crates_map: Default::default(),\n            analyzed_visible_files: None,\n            nonce: Nonce::new(),\n",
    )?;
    fs::write(lib_rs, source)?;
    Ok(())
}

fn patch_search_source(search_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(search_rs)?;

    replace_once(
        &mut source,
        "        let all_crates = all_crates(db);\n        for &krate in all_crates.iter() {\n",
        "        let all_crates = db.analyzed_visible_base_crates(all_crates(db).iter().copied());\n        for krate in all_crates {\n",
    )?;
    replace_once(
        &mut source,
        "        for rev_dep in of.transitive_reverse_dependencies(db) {\n",
        "        for rev_dep in db.analyzed_visible_hir_crates(of.transitive_reverse_dependencies(db)) {\n",
    )?;

    fs::write(search_rs, source)?;
    Ok(())
}

fn patch_symbol_index_source(symbol_index_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(symbol_index_rs)?;

    replace_once(
        &mut source,
        "fn resolve_path_to_modules(\n    db: &dyn HirDatabase,\n",
        "fn resolve_path_to_modules(\n    db: &RootDatabase,\n",
    )?;
    replace_once(
        &mut source,
        "    let matching_crates: Vec<Crate> = Crate::all(db)\n",
        "    let matching_crates: Vec<Crate> = db.analyzed_visible_hir_crates(Crate::all(db))\n",
    )?;
    replace_once(
        &mut source,
        "            for &krate in source_root_crates(db, root).iter() {\n",
        "            for krate in db.analyzed_visible_base_crates(source_root_crates(db, root).iter().copied()) {\n",
    )?;
    replace_once(
        &mut source,
        "            crates.extend(source_root_crates(db, root).iter().copied())\n",
        "            crates.extend(db.analyzed_visible_base_crates(source_root_crates(db, root).iter().copied()))\n",
    )?;
    replace_once(
        &mut source,
        "                    if non_type_for_type_only_query || !self.matches_assoc_mode(symbol.is_assoc) {\n                        continue;\n                    }\n",
        "                    if non_type_for_type_only_query || !self.matches_assoc_mode(symbol.is_assoc) {\n                        continue;\n                    }\n                    if !crate::analyzed::is_symbol_visible(db, symbol) {\n                        continue;\n                    }\n",
    )?;

    fs::write(symbol_index_rs, source)?;
    Ok(())
}

fn patch_node_ext_source(node_ext_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(node_ext_rs)?;

    replace_once(
        &mut source,
        "            Some(ty) =>
            {
                #[expect(
                    clippy::collapsible_match,
                    reason = \"it won't compile due to exhaustiveness\"
                )]
                if cb(ty) {
",
        "            Some(ty) => {
                if cb(ty) {
",
    )?;

    fs::write(node_ext_rs, source)?;
    Ok(())
}

fn owned_source_path(file_name: &str) -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"))
        .join("src")
        .join(file_name)
}
