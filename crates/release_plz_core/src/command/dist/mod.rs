//! Binary distributions with user-owned CI. Build on each runner, collect the
//! artifacts, then publish an existing GitHub draft only when every target is ready.

mod build;
mod installer;
mod publish;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, path::Path, process::Command};

use anyhow::{Context as _, ensure};
use cargo_metadata::{Metadata, Package};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub use build::{BuildOptions, build};
pub use publish::{PreparedDist, prepare, publish};

const SCHEMA_VERSION: u32 = 1;
const MANIFEST_NAME: &str = "dist-manifest.json";
const NOTES_NAME: &str = "dist-notes.md";
const CHECKSUMS_NAME: &str = "sha256.sum";

/// A distribution plan for one build target or the complete set being published.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema_version: u32,
    pub package: String,
    pub version: String,
    pub tag: String,
    pub commit: String,
    pub binaries: Vec<String>,
    pub targets: Vec<String>,
}

impl Plan {
    /// Select a workspace binary package and validate the complete target list.
    /// Planning reads metadata and HEAD, but does not require the tag to exist yet.
    pub fn new(
        metadata: &Metadata,
        package_name: Option<&str>,
        targets: &[String],
        tag: &str,
    ) -> anyhow::Result<Self> {
        let packages: Vec<_> = metadata
            .workspace_packages()
            .into_iter()
            .filter(|p| package_name.is_none_or(|name| p.name.as_str() == name))
            .filter(|p| p.targets.iter().any(|t| t.is_bin()))
            .collect();
        ensure!(
            packages.len() == 1,
            "select one workspace package with binaries using --package"
        );
        ensure!(
            !targets.is_empty(),
            "specify at least one distribution target with --target"
        );
        let package = packages[0];
        validate_component(package.name.as_str())?;
        let mut binaries: Vec<_> = package
            .targets
            .iter()
            .filter(|t| t.is_bin())
            .map(|t| t.name.clone())
            .collect();
        binaries.sort();
        for name in &binaries {
            validate_component(name)?;
        }
        let mut sorted_targets = targets.to_vec();
        sorted_targets.sort();
        sorted_targets.dedup();
        ensure!(
            sorted_targets.len() == targets.len(),
            "duplicate distribution targets"
        );
        for target in targets {
            validate_component(target)?;
            ensure!(
                target.contains('-'),
                "expected a Rust target triple: {target}"
            );
        }
        let root = metadata.workspace_root.as_std_path();
        git(root, &["check-ref-format", &format!("refs/tags/{tag}")])?;
        let commit = git(root, &["rev-parse", "HEAD"])?;
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            package: package.name.to_string(),
            version: package.version.to_string(),
            tag: tag.to_owned(),
            commit,
            binaries,
            targets: sorted_targets,
        })
    }

    /// Require a clean checkout of the requested tag, including annotated tags.
    pub fn verify_checkout(&self, root: &Path) -> anyhow::Result<()> {
        ensure!(
            git(root, &["rev-parse", "HEAD"])? == self.commit,
            "HEAD changed since the distribution was planned"
        );
        let tagged_commit = git(
            root,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/tags/{}^{{commit}}", self.tag),
            ],
        )
        .context("check out the release tag before distributing binaries")?;
        ensure!(
            tagged_commit == self.commit,
            "HEAD does not match release tag {}",
            self.tag
        );
        ensure!(
            git(root, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty(),
            "distribution requires a clean checkout of the release tag"
        );
        Ok(())
    }

    pub fn package_metadata<'a>(&self, metadata: &'a Metadata) -> anyhow::Result<&'a Package> {
        metadata
            .workspace_packages()
            .into_iter()
            .find(|p| p.name.as_str() == self.package)
            .context("distribution package is no longer in the workspace")
    }

    /// Archive names are independent of the version to preserve existing URLs.
    pub fn archives(&self, target: &str) -> Vec<String> {
        let stem = format!("{}-{target}", self.package);
        let mut names = vec![format!("{stem}.tar.gz")];
        if is_windows(target) {
            names.push(format!("{stem}.zip"));
        }
        names
    }

    fn target_manifest_name(&self, target: &str) -> String {
        format!("{}-{target}.{MANIFEST_NAME}", self.package)
    }

    /// Build runners only need to know their own target, not the full CI matrix.
    fn for_target(&self, target: &str) -> Self {
        Self {
            targets: vec![target.to_owned()],
            ..self.clone()
        }
    }
}

/// An archive's content digest, used to verify files transferred between runners.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub target: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TargetManifest {
    plan: Plan,
    target: String,
    artifacts: BTreeMap<String, Artifact>,
}

/// A generated installer's content digest.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Installer {
    pub sha256: String,
    pub size: u64,
}

/// Published manifest containing the validated archives and generated installers.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plan: Plan,
    pub artifacts: BTreeMap<String, Artifact>,
    pub installers: BTreeMap<String, Installer>,
}

fn is_windows(target: &str) -> bool {
    target.split('-').any(|part| part == "windows")
}

fn validate_component(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid distribution name or target: {value}"
    );
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .context("failed to run git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn digest(path: &Path) -> anyhow::Result<(String, u64)> {
    let mut file = fs_err::File::open(path)?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher)?;
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    fs_err::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
