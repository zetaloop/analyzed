use analyzed_bridge as build_support;

use std::{error::Error, fs, path::Path};

use analyzed_bridge::replace_once;

const PACKAGE: &str = "ra_ap_base_db";
const GENERATED_DIR: &str = "ra_ap_base_db_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_base_db_source(&generated.join("src/lib.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_base_db_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

    replace_once(
        &mut source,
        "    fn crates_map(&self) -> Arc<CratesMap>;\n",
        "    fn analyzed_is_crate_visible(&self, _krate: Crate) -> bool {\n        true\n    }\n\n    fn analyzed_crate_visibility_key(&self) -> u64 {\n        0\n    }\n\n    fn crates_map(&self) -> Arc<CratesMap>;\n",
    )?;
    replace_once(
        &mut source,
        "pub fn all_crates(db: &dyn salsa::Database) -> std::sync::Arc<[Crate]> {\n    AllCrates::try_get(db).map_or(std::sync::Arc::default(), |all_crates| all_crates.crates(db))\n}\n",
        "pub fn all_crates(db: &dyn SourceDatabase) -> std::sync::Arc<[Crate]> {\n    let crates = AllCrates::try_get(db).map_or(std::sync::Arc::default(), |all_crates| all_crates.crates(db));\n    let filtered = crates\n        .iter()\n        .copied()\n        .filter(|&krate| db.analyzed_is_crate_visible(krate))\n        .collect::<Vec<_>>();\n\n    if filtered.len() == crates.len() {\n        crates\n    } else {\n        filtered.into()\n    }\n}\n",
    )?;
    replace_once(
        &mut source,
        "        db: &'db dyn SourceDatabase,\n        id: InternedSourceRootId<'db>,\n    ) -> Box<[Crate]> {\n        let crates = AllCrates::get(db).crates(db);\n        let id = id.id(db);\n",
        "        db: &'db dyn SourceDatabase,\n        id: InternedSourceRootId<'db>,\n        _visibility: u64,\n    ) -> Box<[Crate]> {\n        let crates = all_crates(db);\n        let id = id.id(db);\n",
    )?;
    replace_once(
        &mut source,
        "    source_root_crates(db, InternedSourceRootId::new(db, id))\n",
        "    source_root_crates(db, InternedSourceRootId::new(db, id), db.analyzed_crate_visibility_key())\n",
    )?;

    fs::write(lib_rs, source)?;
    Ok(())
}
