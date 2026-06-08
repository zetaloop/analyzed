use analyzed_bridge as build_support;

use std::{error::Error, fs, path::Path};

use analyzed_bridge::replace_once;

const PACKAGE: &str = "ra_ap_ide_db";
const GENERATED_DIR: &str = "ra_ap_ide_db_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (generated, _) = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    patch_ide_db_source(&generated.join("src/lib.rs"))?;
    patch_ra_fixture_source(&generated.join("src/ra_fixture.rs"))?;
    patch_node_ext_source(&generated.join("src/syntax_helpers/node_ext.rs"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

fn patch_ide_db_source(lib_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(lib_rs)?;

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
        "    fn crates_map(&self) -> Arc<CratesMap> {\n        self.crates_map.clone()\n    }\n",
        "    fn analyzed_is_crate_visible(&self, krate: base_db::Crate) -> bool {\n        self.analyzed_visible_files.as_ref().is_none_or(|visible_files| {\n            visible_files.contains(&krate.data(self).root_file_id)\n        })\n    }\n\n    fn analyzed_crate_visibility_key(&self) -> u64 {\n        let Some(visible_files) = &self.analyzed_visible_files else {\n            return 0;\n        };\n        let mut files = visible_files.iter().map(|file| file.index()).collect::<Vec<_>>();\n        files.sort_unstable();\n        let mut key = files.len() as u64;\n        for file in files {\n            key = key.wrapping_mul(1_099_511_628_211).wrapping_add(file as u64);\n        }\n        key\n    }\n\n    fn crates_map(&self) -> Arc<CratesMap> {\n        self.crates_map.clone()\n    }\n",
    )?;
    replace_once(
        &mut source,
        "            files: Default::default(),\n            crates_map: Default::default(),\n            nonce: Nonce::new(),\n",
        "            files: Default::default(),\n            crates_map: Default::default(),\n            analyzed_visible_files: None,\n            nonce: Nonce::new(),\n",
    )?;
    replace_once(
        &mut source,
        "    pub fn enable_proc_attr_macros(&mut self) {\n",
        "    pub fn analyzed_with_visible_files(\n        mut self,\n        visible_files: std::sync::Arc<rustc_hash::FxHashSet<vfs::FileId>>,\n    ) -> RootDatabase {\n        self.analyzed_visible_files = Some(visible_files);\n        self\n    }\n\n    pub fn enable_proc_attr_macros(&mut self) {\n",
    )?;
    replace_once(
        &mut source,
        "        Self(test_utils::MiniCore::RAW_SOURCE)\n",
        "        Self(\"\")\n",
    )?;
    replace_once(
        &mut source,
        "        if self.0 == test_utils::MiniCore::RAW_SOURCE {\n",
        "        if self.0.is_empty() {\n",
    )?;

    fs::write(lib_rs, source)?;
    Ok(())
}

fn patch_ra_fixture_source(ra_fixture_rs: &Path) -> Result<(), Box<dyn Error>> {
    let mut source = fs::read_to_string(ra_fixture_rs)?;

    replace_once(
        &mut source,
        "        // We don't want a mistake in the fixture to crash r-a, so we wrap this in `catch_unwind()`.
        std::panic::catch_unwind(|| {
            let mut db = RootDatabase::default();
            let fixture =
                test_fixture::ChangeFixture::parse_with_proc_macros(text, minicore.0, Vec::new());
            db.apply_change(fixture.change);
            let files = fixture
                .files
                .into_iter()
                .zip(fixture.file_lines)
                .map(|(file_id, range)| (file_id.file_id(), range))
                .collect();
            (db, files, fixture.sysroot_files)
        })
        .map_err(|error| {
            tracing::error!(
                \"cannot crate the crate graph: {}\\nCrate graph:\\n{}\\n\",
                if let Some(&s) = error.downcast_ref::<&'static str>() {
                    s
                } else if let Some(s) = error.downcast_ref::<String>() {
                    s.as_str()
                } else {
                    \"Box<dyn Any>\"
                },
                text,
            );
        })
",
        "        let _ = (text, minicore);
        Err(())
",
    )?;

    fs::write(ra_fixture_rs, source)?;
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
