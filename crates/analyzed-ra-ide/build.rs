use analyzed_bridge as build_support;

use std::{error::Error, fs, path::Path};

use analyzed_bridge::replace_once;

const PACKAGE: &str = "ra_ap_ide";
const GENERATED_DIR: &str = "ra_ap_ide_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_ide_source(&generated.join("src/lib.rs"))?;
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
