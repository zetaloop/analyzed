use std::{
    env,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use flate2::{Compression, write::GzEncoder};
use time::OffsetDateTime;
use xshell::{Cmd, Shell, cmd};
use zip::{DateTime, ZipWriter, write::SimpleFileOptions};

// The release facts below follow the upstream release configuration.
struct TargetSpec {
    triple: &'static str,
    runner: &'static str,
    container: Option<&'static str>,
    zig_glibc: Option<&'static str>,
    features: &'static [&'static str],
    rustflags: &'static [&'static str],
    pgo: bool,
}

const TARGETS: &[TargetSpec] = &[
    TargetSpec {
        triple: "x86_64-pc-windows-msvc",
        runner: "windows-latest",
        container: None,
        zig_glibc: None,
        features: &["mimalloc"],
        rustflags: &["-Ctarget-feature=+crt-static"],
        pgo: true,
    },
    TargetSpec {
        triple: "i686-pc-windows-msvc",
        runner: "windows-latest",
        container: None,
        zig_glibc: None,
        features: &["mimalloc"],
        rustflags: &["-Ctarget-feature=+crt-static"],
        pgo: true,
    },
    TargetSpec {
        triple: "aarch64-pc-windows-msvc",
        runner: "windows-latest",
        container: None,
        zig_glibc: None,
        features: &["mimalloc"],
        rustflags: &["-Ctarget-feature=+crt-static"],
        pgo: false,
    },
    TargetSpec {
        triple: "x86_64-unknown-linux-gnu",
        runner: "ubuntu-latest",
        container: Some("quay.io/pypa/manylinux_2_28_x86_64"),
        zig_glibc: None,
        features: &[],
        rustflags: &[],
        pgo: true,
    },
    TargetSpec {
        triple: "aarch64-unknown-linux-gnu",
        runner: "ubuntu-24.04-arm",
        container: Some("quay.io/pypa/manylinux_2_28_aarch64"),
        zig_glibc: None,
        features: &[],
        rustflags: &[],
        pgo: true,
    },
    TargetSpec {
        triple: "arm-unknown-linux-gnueabihf",
        runner: "ubuntu-latest",
        container: None,
        zig_glibc: Some("2.28"),
        features: &[],
        rustflags: &[],
        pgo: false,
    },
    TargetSpec {
        triple: "x86_64-apple-darwin",
        runner: "macos-14",
        container: None,
        zig_glibc: None,
        features: &[],
        rustflags: &[],
        pgo: true,
    },
    TargetSpec {
        triple: "aarch64-apple-darwin",
        runner: "macos-14",
        container: None,
        zig_glibc: None,
        features: &[],
        rustflags: &[],
        pgo: true,
    },
    TargetSpec {
        triple: "x86_64-unknown-linux-musl",
        runner: "ubuntu-latest",
        container: Some("rust:alpine"),
        zig_glibc: None,
        features: &[],
        // the dynamic musl link needs lld under the alpine clang toolchain
        rustflags: &["-Clink-arg=-fuse-ld=lld", "-Ctarget-feature=-crt-static"],
        pgo: false,
    },
];

pub(crate) fn run(sh: &Shell, training_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let target = Target::detect(sh)?;
    let dist = project_root().join("dist");
    sh.remove_path(&dist)?;
    sh.create_dir(&dist)?;

    let _lto = sh.push_env("CARGO_PROFILE_RELEASE_LTO", "thin");
    let _codegen_units = sh.push_env("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1");
    let _deployment_target = target
        .spec
        .triple
        .ends_with("-apple-darwin")
        .then(|| sh.push_env("MACOSX_DEPLOYMENT_TARGET", "14.0"));

    build(sh, &target, training_dir)?;
    package(&target, &dist)
}

pub(crate) fn matrix() {
    let include: Vec<serde_json::Value> = TARGETS
        .iter()
        .map(|spec| {
            serde_json::json!({
                "target": spec.triple,
                "os": spec.runner,
                "container": spec.container,
                "zig": spec.zig_glibc.is_some(),
                "pgo": spec.pgo,
            })
        })
        .collect();
    println!("{}", serde_json::json!({ "include": include }));
}

fn build(sh: &Shell, target: &Target, training_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let command = if target.spec.zig_glibc.is_some() {
        "zigbuild"
    } else {
        "build"
    };
    let cargo_target = &target.cargo_target;
    let features: Vec<&str> = target
        .spec
        .features
        .iter()
        .flat_map(|feature| ["--features", feature])
        .collect();
    let features = features.as_slice();
    let rustflags = target.spec.rustflags.join(" ");

    if !target.spec.pgo {
        with_rustflags(
            cmd!(
                sh,
                "cargo {command} -p analyzed --target {cargo_target} {features...} --release"
            ),
            &rustflags,
        )
        .run()
        .context("cannot build analyzed")?;
        return Ok(());
    }

    let training_dir = training_dir.context(
        "this target builds with PGO; pass --training-dir with a checkout of the pinned training crate",
    )?;
    anyhow::ensure!(
        training_dir.is_dir(),
        "training directory {} does not exist",
        training_dir.display()
    );
    cmd!(sh, "cargo pgo --version").quiet().ignore_stdout().run().context(
        "cargo-pgo is required for PGO builds; install it with `cargo install --locked cargo-pgo`",
    )?;

    with_rustflags(
        cmd!(
            sh,
            "cargo pgo build -- -p analyzed --target {cargo_target} {features...}"
        ),
        &rustflags,
    )
    .run()
    .context("cannot build analyzed with PGO instrumentation")?;

    let server_path = &target.server_path;
    cmd!(
        sh,
        "{server_path} analysis-stats -q --run-all-ide-things {training_dir}"
    )
    .run()
    .context("cannot gather PGO profiles")?;

    with_rustflags(
        cmd!(
            sh,
            "cargo pgo optimize build -- -p analyzed --target {cargo_target} {features...}"
        ),
        &rustflags,
    )
    .run()
    .context("cannot build analyzed with PGO optimization")?;
    Ok(())
}

fn with_rustflags<'a>(cmd: Cmd<'a>, rustflags: &str) -> Cmd<'a> {
    if rustflags.is_empty() {
        cmd
    } else {
        cmd.env("RUSTFLAGS", rustflags)
    }
}

fn package(target: &Target, dist: &Path) -> anyhow::Result<()> {
    let triple = target.spec.triple;
    match &target.symbols_path {
        Some(symbols_path) => zip(
            &target.server_path,
            symbols_path,
            &dist.join(format!("analyzed-{triple}.zip")),
        ),
        None => tar_gz(
            &target.server_path,
            &dist.join(format!("analyzed-{triple}.tar.gz")),
        ),
    }
}

fn tar_gz(server_path: &Path, dest_path: &Path) -> anyhow::Result<()> {
    let encoder = GzEncoder::new(
        BufWriter::new(File::create(dest_path)?),
        Compression::best(),
    );
    let mut builder = tar::Builder::new(encoder);
    builder.append_path_with_name(server_path, "analyzed")?;
    builder.into_inner()?.finish()?.flush()?;
    Ok(())
}

fn zip(server_path: &Path, symbols_path: &Path, dest_path: &Path) -> anyhow::Result<()> {
    let mut writer = ZipWriter::new(BufWriter::new(File::create(dest_path)?));
    for (path, executable) in [(server_path, true), (symbols_path, false)] {
        let mut options = SimpleFileOptions::default()
            .last_modified_time(DateTime::try_from(OffsetDateTime::from(
                fs::metadata(path)?.modified()?,
            ))?)
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9));
        if executable {
            options = options.unix_permissions(0o755);
        }
        writer.start_file(path.file_name().unwrap().to_str().unwrap(), options)?;
        io::copy(&mut File::open(path)?, &mut writer)?;
    }
    writer.finish()?.flush()?;
    Ok(())
}

struct Target {
    spec: &'static TargetSpec,
    cargo_target: String,
    server_path: PathBuf,
    symbols_path: Option<PathBuf>,
}

impl Target {
    fn detect(sh: &Shell) -> anyhow::Result<Self> {
        let triple = match env::var("ANALYZED_TARGET") {
            Ok(triple) => triple,
            Err(_) => cmd!(sh, "rustc --print=host-tuple")
                .read()
                .context("cannot detect the host target; set ANALYZED_TARGET")?,
        };
        let spec = TARGETS
            .iter()
            .find(|spec| spec.triple == triple)
            .with_context(|| format!("{triple} is not a release target"))?;
        let cargo_target = match spec.zig_glibc {
            Some(glibc) => format!("{}.{glibc}", spec.triple),
            None => spec.triple.to_owned(),
        };
        let out_dir = project_root()
            .join("target")
            .join(spec.triple)
            .join("release");
        let (server_path, symbols_path) = if spec.triple.contains("-windows-") {
            (
                out_dir.join("analyzed.exe"),
                Some(out_dir.join("analyzed.pdb")),
            )
        } else {
            (out_dir.join("analyzed"), None)
        };
        Ok(Target {
            spec,
            cargo_target,
            server_path,
            symbols_path,
        })
    }
}

pub(crate) fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}
