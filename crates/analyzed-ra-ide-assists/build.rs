use analyzed_bridge as build_support;

const PACKAGE: &str = "ra_ap_ide_assists";
const GENERATED_DIR: &str = "ra_ap_ide_assists_bridge";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = build_support::prepare_bridge_package(PACKAGE, GENERATED_DIR)?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
