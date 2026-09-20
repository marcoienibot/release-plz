use std::{
    collections::BTreeMap,
    io::{BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, ensure};
use cargo_metadata::{Message, Metadata};
use flate2::{Compression, write::GzEncoder};
use zip::write::SimpleFileOptions;

use super::{Artifact, Plan, TargetManifest, digest, is_windows, write_json};

#[derive(Debug)]
pub struct BuildOptions {
    pub output_dir: PathBuf,
    pub features: Vec<String>,
    pub no_default_features: bool,
}

/// Build all binaries in the selected package, then stage archives and a manifest.
pub fn build(plan: &Plan, metadata: &Metadata, options: &BuildOptions) -> anyhow::Result<()> {
    let [target] = plan.targets.as_slice() else {
        anyhow::bail!("dist build requires exactly one target");
    };
    plan.verify_checkout(metadata.workspace_root.as_std_path())?;
    let package = plan.package_metadata(metadata)?;
    fs_err::create_dir_all(&options.output_dir)?;
    // A failed rebuild must not leave a previous successful manifest behind.
    let manifest_path = options.output_dir.join(plan.target_manifest_name(target));
    if manifest_path.exists() {
        fs_err::remove_file(&manifest_path)?;
    }
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .current_dir(&metadata.workspace_root)
        .args([
            "--release",
            "--locked",
            "--bins",
            "--message-format=json-render-diagnostics",
            "--package",
            &plan.package,
            "--target",
            target,
            "--manifest-path",
        ])
        .arg(&package.manifest_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if !options.features.is_empty() {
        command.arg("--features").arg(options.features.join(","));
    }
    if options.no_default_features {
        command.arg("--no-default-features");
    }
    if target.ends_with("-windows-msvc") {
        // --target keeps these flags away from host build scripts/proc-macros.
        // Preserve encoded arguments (including spaces) and Cargo's precedence.
        if let Ok(flags) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
            let separator = if flags.is_empty() { "" } else { "\x1f" };
            command.env(
                "CARGO_ENCODED_RUSTFLAGS",
                format!("{flags}{separator}-C\x1ftarget-feature=+crt-static"),
            );
        } else if let Ok(flags) = std::env::var("RUSTFLAGS") {
            command.env(
                "RUSTFLAGS",
                format!("{flags} -C target-feature=+crt-static"),
            );
        } else {
            command.arg("--config").arg(format!(
                "target.{target}.rustflags=[\"-C\",\"target-feature=+crt-static\"]"
            ));
        }
    }
    tracing::info!("building {} for {target}", plan.package);
    let mut child = command
        .spawn()
        .context("failed to start cargo build; install Cargo and the target toolchain")?;
    let stdout = child.stdout.take().context("missing builder output")?;
    let mut binaries = BTreeMap::new();
    let mut parse_error = None;
    for message in Message::parse_stream(BufReader::new(stdout)) {
        match message {
            Ok(Message::CompilerArtifact(artifact))
                if artifact.package_id == package.id
                    && artifact.target.is_bin()
                    && !artifact.profile.test =>
            {
                if let Some(path) = artifact.executable {
                    binaries.insert(artifact.target.name, path.into_std_path_buf());
                }
            }
            Ok(Message::CompilerMessage(message)) => {
                if let Some(rendered) = message.message.rendered {
                    eprint!("{rendered}");
                }
            }
            Ok(Message::TextLine(line)) => eprintln!("{line}"),
            Err(error) => {
                parse_error = Some(error);
            }
            _ => {}
        }
    }
    let status = child.wait().context("failed to wait for binary builder")?;
    ensure!(status.success(), "binary build failed for {target}");
    if let Some(error) = parse_error {
        return Err(error.into());
    }
    ensure!(
        binaries.keys().eq(plan.binaries.iter()),
        "builder did not produce all package binaries; enable their required features with --features"
    );
    // Cargo/build scripts must not have modified tracked sources while building.
    plan.verify_checkout(metadata.workspace_root.as_std_path())?;
    stage(plan, target, &options.output_dir, &binaries)
}

pub(super) fn stage(
    plan: &Plan,
    target: &str,
    directory: &Path,
    binaries: &BTreeMap<String, PathBuf>,
) -> anyhow::Result<()> {
    fs_err::create_dir_all(directory)?;
    let mut artifacts = BTreeMap::new();
    for name in plan.archives(target) {
        let path = directory.join(&name);
        let temporary = tempfile::NamedTempFile::new_in(directory)?;
        if name.ends_with(".zip") {
            let mut archive = zip::ZipWriter::new(temporary.as_file());
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755);
            for (binary, source) in binaries {
                archive.start_file(binary_filename(binary, target), options)?;
                std::io::copy(&mut fs_err::File::open(source)?, &mut archive)?;
            }
            archive.finish()?.flush()?;
        } else {
            let encoder = GzEncoder::new(temporary.as_file(), Compression::default());
            let mut archive = tar::Builder::new(encoder);
            for (binary, source) in binaries {
                let mut file = fs_err::File::open(source)?;
                let mut header = tar::Header::new_gnu();
                header.set_size(file.metadata()?.len());
                header.set_mode(0o755);
                header.set_mtime(0);
                header.set_cksum();
                archive.append_data(&mut header, binary_filename(binary, target), &mut file)?;
            }
            archive.into_inner()?.finish()?.flush()?;
        }
        temporary.persist(&path)?;
        let (sha256, size) = digest(&path)?;
        fs_err::write(
            directory.join(format!("{name}.sha256")),
            format!("{sha256}  {name}\n"),
        )?;
        artifacts.insert(
            name,
            Artifact {
                target: target.to_owned(),
                sha256,
                size,
            },
        );
    }
    // Written last: its presence means every archive for this target is complete.
    write_json(
        &directory.join(plan.target_manifest_name(target)),
        &TargetManifest {
            plan: plan.for_target(target),
            target: target.to_owned(),
            artifacts,
        },
    )
}

fn binary_filename(binary: &str, target: &str) -> String {
    if is_windows(target) {
        format!("{binary}.exe")
    } else {
        binary.to_owned()
    }
}
