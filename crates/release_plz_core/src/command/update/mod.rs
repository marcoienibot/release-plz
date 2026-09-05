mod changelog_update;
mod package_dependencies;
mod packages_update;
mod update_config;
pub mod update_request;
pub mod updater;

use crate::{PackagePath, tmp_repo::TempRepo};
use crate::{fs_utils, root_repo_path_from_manifest_dir};
use anyhow::Context;
use cargo_metadata::camino::Utf8Path;
use cargo_metadata::{Package, semver::Version};
use cargo_utils::LocalManifest;
use cargo_utils::{CARGO_TOML, upgrade_requirement};
use git_cmd::Repo;
use serde::{Deserialize, Serialize};
use std::iter;
use tracing::{info, warn};
use update_request::UpdateRequest;

use tracing::{debug, instrument};

pub use packages_update::*;
pub use update_config::*;

#[derive(Serialize, Deserialize, Debug)]
pub struct ReleaseInfo {
    /// Package name
    package: String,
    pub title: Option<String>,
    pub changelog: Option<String>,
    previous_version: String,
    next_version: String,
    /// Summary of breaking changes of the release
    breaking_changes: Option<String>,
    semver_check: String,
}

/// Update a local Rust project.
#[instrument(skip_all)]
pub async fn update(input: &UpdateRequest) -> anyhow::Result<(PackagesUpdate, TempRepo)> {
    let (packages_to_update, repository) = crate::next_versions(input)
        .await
        .context("failed to determine next versions")?;
    let local_manifest_path = input.local_manifest();
    let local_metadata = cargo_utils::get_manifest_metadata(local_manifest_path)?;
    // Read packages from `local_metadata` to update the manifest of local
    // workspace dependencies.
    let all_packages: Vec<Package> = cargo_utils::workspace_members(&local_metadata)?.collect();
    let all_packages_ref: Vec<&Package> = all_packages.iter().collect();
    update_manifests(
        &packages_to_update,
        local_manifest_path,
        &all_packages_ref,
        input.should_update_local_dependencies(),
    )?;
    update_changelogs(input, &packages_to_update)?;
    if !packages_to_update.updates().is_empty() {
        let local_manifest_dir = input.local_manifest_dir()?;
        update_cargo_lock(local_manifest_dir, input.should_update_dependencies())?;

        let local_repo_root = root_repo_path_from_manifest_dir(local_manifest_dir)?;
        let there_are_commits_to_push = Repo::new(local_repo_root)?.is_clean().is_err();
        if !there_are_commits_to_push {
            info!("the repository is already up-to-date");
        }
    }

    Ok((packages_to_update, repository))
}

fn update_manifests(
    packages_to_update: &PackagesUpdate,
    local_manifest_path: &Utf8Path,
    all_packages: &[&Package],
    local_dependencies_update: bool,
) -> anyhow::Result<()> {
    if !local_dependencies_update {
        let mut changes = Vec::new();
        for package in all_packages {
            let version = packages_to_update
                .updates()
                .iter()
                .find(|(updated, _)| updated.name == package.name)
                .map(|(_, update)| &update.version);
            let inherited = LocalManifest::try_new(&package.manifest_path)?.version_is_inherited();
            if let Some(version) = version.or_else(|| {
                inherited
                    .then(|| packages_to_update.workspace_version())
                    .flatten()
            }) {
                changes.push((*package, version));
            }
        }
        ensure_retained_requirements_match(all_packages, &changes, local_manifest_path)?;
    }
    // Distinguish packages type to avoid updating the version of packages that inherit the workspace version
    let (workspace_pkgs, independent_pkgs): (PackagesToUpdate, PackagesToUpdate) =
        packages_to_update
            .updates_clone()
            .into_iter()
            .partition(|(p, _)| {
                let local_manifest_path = p.package_path().unwrap().join(CARGO_TOML);
                let local_manifest = LocalManifest::try_new(&local_manifest_path).unwrap();
                local_manifest.version_is_inherited()
            });

    if let Some(new_workspace_version) = packages_to_update.workspace_version() {
        let mut local_manifest = LocalManifest::try_new(local_manifest_path)?;
        local_manifest.set_workspace_version(new_workspace_version);
        local_manifest
            .write()
            .context("can't update workspace version")?;

        for (pkg, _) in workspace_pkgs {
            if !local_dependencies_update {
                continue;
            }
            let package_path = pkg.package_path()?;
            update_dependencies(
                all_packages,
                new_workspace_version,
                package_path,
                local_manifest_path,
            )?;
        }
    }

    update_versions(
        all_packages,
        &PackagesUpdate::new(independent_pkgs),
        local_manifest_path,
        local_dependencies_update,
    )?;
    Ok(())
}

#[instrument(skip_all)]
fn update_versions(
    all_packages: &[&Package],
    packages_to_update: &PackagesUpdate,
    workspace_manifest: &Utf8Path,
    local_dependencies_update: bool,
) -> anyhow::Result<()> {
    for (package, update) in packages_to_update.updates() {
        let package_path = package.package_path()?;
        set_version_with_dependencies(
            all_packages,
            package_path,
            &update.version,
            workspace_manifest,
            local_dependencies_update,
        )?;
    }
    Ok(())
}

#[instrument(skip_all)]
fn update_changelogs(
    update_request: &UpdateRequest,
    local_packages: &PackagesUpdate,
) -> anyhow::Result<()> {
    for (package, update) in local_packages.updates() {
        if let Some(changelog) = update.changelog.as_ref() {
            let changelog_path = update_request.changelog_path(package);
            fs_err::write(&changelog_path, changelog).context("cannot write changelog")?;
        }
    }
    Ok(())
}

#[instrument(skip_all)]
pub(crate) fn update_cargo_lock(
    root: &Utf8Path,
    update_all_dependencies: bool,
) -> anyhow::Result<()> {
    let mut args = vec!["update"];
    if !update_all_dependencies {
        args.push("--workspace");
    }
    let output = crate::cargo::run_cargo(root, &args)
        .context("error while running cargo to update the Cargo.lock file")?;

    anyhow::ensure!(
        output.status.success(),
        "cargo update failed. stdout: {}; stderr: {}",
        output.stdout,
        output.stderr
    );

    Ok(())
}

#[instrument(skip(all_packages))]
pub fn set_version(
    all_packages: &[&Package],
    package_path: &Utf8Path,
    version: &Version,
    workspace_manifest: &Utf8Path,
) -> anyhow::Result<()> {
    set_version_with_dependencies(
        all_packages,
        package_path,
        version,
        workspace_manifest,
        true,
    )
}

pub(crate) fn set_version_with_dependencies(
    all_packages: &[&Package],
    package_path: &Utf8Path,
    version: &Version,
    workspace_manifest: &Utf8Path,
    local_dependencies_update: bool,
) -> anyhow::Result<()> {
    debug!("updating version");
    let mut local_manifest =
        LocalManifest::try_new(&package_path.join("Cargo.toml")).context("cannot read manifest")?;
    local_manifest.set_package_version(version);
    local_manifest
        .write()
        .with_context(|| format!("cannot update manifest {:?}", local_manifest.path))?;

    let package_path = fs_utils::canonicalize_utf8(crate::manifest_dir(&local_manifest.path)?)?;
    if local_dependencies_update {
        update_dependencies(all_packages, version, &package_path, workspace_manifest)?;
    }
    Ok(())
}

/// Validate the complete plan before changing any manifest or changelog.
pub(crate) fn ensure_retained_requirements_match(
    all_packages: &[&Package],
    changes: &[(&Package, &Version)],
    workspace_manifest_path: &Utf8Path,
) -> anyhow::Result<()> {
    let workspace_manifest = LocalManifest::try_new(workspace_manifest_path)?;
    let workspace_dependencies = workspace_manifest.get_workspace_dependency_table();
    for dependent in all_packages {
        let manifest = LocalManifest::try_new(&dependent.manifest_path)?;
        for dependency in &dependent.dependencies {
            // Metadata represents both a path-only dependency and an explicit
            // wildcard as `*`; only an explicit requirement constrains a bump.
            let name = dependency.rename.as_deref().unwrap_or(&dependency.name);
            let explicit_requirement = manifest
                .get_dependency_tables()
                .filter_map(|table| table.get(name).and_then(|item| item.as_table_like()))
                .any(|table| {
                    table.contains_key("version")
                        || (table.get("workspace").and_then(|item| item.as_bool()) == Some(true)
                            && workspace_dependencies
                                .and_then(|table| table.get(name))
                                .and_then(|item| item.as_table_like())
                                .is_some_and(|table| table.contains_key("version")))
                });
            if !explicit_requirement {
                continue;
            }
            for (package, version) in changes {
                if dependency.path.as_deref() == package.manifest_path.parent()
                    && dependency.name == package.name.as_str()
                {
                    anyhow::ensure!(
                        dependency.req.matches(version),
                        "local_dependencies_update=false would leave `{}` depending on `{}` {} but its new workspace version is {}; update that requirement explicitly or enable local_dependencies_update before releasing",
                        dependent.name,
                        dependency.name,
                        dependency.req,
                        version
                    );
                }
            }
        }
    }
    Ok(())
}

/// Update the package version in the dependencies of the other packages.
/// E.g. from:
///
/// ```toml
/// [dependencies]
/// pkg1 = { path = "../pkg1", version = "1.2.3" }
/// ```
///
/// to:
///
/// ```toml
/// [dependencies]
/// pkg1 = { path = "../pkg1", version = "1.2.4" }
/// ```
///
/// Works also for the dependencies in a workspace:
///
/// ```toml
/// [workspace.dependencies]
/// pkg1 = { path = "../pkg1", version = "1.2.4" }
/// ```
///
fn update_dependencies(
    all_packages: &[&Package],
    version: &Version,
    package_path: &Utf8Path,
    workspace_manifest: &Utf8Path,
) -> anyhow::Result<()> {
    let all_manifests = iter::once(workspace_manifest)
        .chain(all_packages.iter().map(|pkg| pkg.manifest_path.as_path()));
    for manifest in all_manifests {
        let mut local_manifest = LocalManifest::try_new(manifest)?;
        let manifest_dir = crate::manifest_dir(&local_manifest.path)?.to_owned();
        let deps_to_update = local_manifest
            .get_dependency_tables_mut()
            .flat_map(|t| t.iter_mut().filter_map(|(_, d)| d.as_table_like_mut()))
            .filter(|d| d.contains_key("version"))
            .filter(|d| crate::is_dependency_referred_to_package(*d, &manifest_dir, package_path));

        for dep in deps_to_update {
            let old_req = dep
                .get("version")
                .expect("filter ensures this")
                .as_str()
                .unwrap_or("*");
            if let Some(new_req) = upgrade_requirement(old_req, version)? {
                dep.insert("version", toml_edit::value(new_req));
            }
        }
        local_manifest.write()?;
    }
    Ok(())
}

#[cfg(test)]
mod local_dependency_policy_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Vec<Package>) {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        fs_err::write(
            root.join(CARGO_TOML),
            "[workspace]\nmembers=[\"a\",\"b\"]\nresolver=\"2\"\n",
        )
        .unwrap();
        for name in ["a", "b"] {
            fs_err::create_dir_all(root.join(name).join("src")).unwrap();
            let mut manifest =
                format!("[package]\nname=\"{name}\"\nversion=\"1.0.0\"\nedition=\"2021\"\n");
            if name == "b" {
                manifest.push_str(
                    "[dependencies]\nrenamed={package=\"a\",path=\"../a\",version=\"1.0.0\"}\n",
                );
            }
            fs_err::write(root.join(name).join(CARGO_TOML), manifest).unwrap();
            fs_err::write(root.join(name).join("src/lib.rs"), "pub fn api() {}\n").unwrap();
            fs_err::write(
                root.join(name).join("CHANGELOG.md"),
                "## [1.0.0] - 2025-01-01\n\nOriginal notes\n",
            )
            .unwrap();
        }
        let metadata = cargo_utils::get_manifest_metadata(&root.join(CARGO_TOML)).unwrap();
        let packages = cargo_utils::workspace_members(&metadata).unwrap().collect();
        (temp, packages)
    }

    fn updates(package: &Package, version: Version) -> PackagesUpdate {
        PackagesUpdate::new(vec![(
            package.clone(),
            crate::UpdateResult {
                version,
                changelog: None,
                new_changelog_entry: None,
                semver_check: crate::semver_check::SemverCheck::Skipped,
                registry_version: None,
            },
        )])
    }

    #[test]
    fn retained_compatible_requirements_survive_version_and_lockfile_updates() {
        let (temp, packages) = fixture();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let a = packages.iter().find(|package| package.name == "a").unwrap();
        let before = fs_err::read(root.join("b/Cargo.toml")).unwrap();
        update_manifests(
            &updates(a, Version::new(1, 0, 1)),
            &root.join(CARGO_TOML),
            &packages.iter().collect::<Vec<_>>(),
            false,
        )
        .unwrap();
        update_cargo_lock(root, false).unwrap();
        assert_eq!(fs_err::read(root.join("b/Cargo.toml")).unwrap(), before);
        let metadata = cargo_utils::get_manifest_metadata(&root.join(CARGO_TOML)).unwrap();
        assert_eq!(
            metadata
                .packages
                .iter()
                .find(|package| package.name == "a")
                .unwrap()
                .version,
            Version::new(1, 0, 1)
        );
    }

    #[test]
    fn incompatible_retained_requirements_fail_before_manifest_changes() {
        let (temp, packages) = fixture();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let a = packages.iter().find(|package| package.name == "a").unwrap();
        let before = fs_err::read(root.join("a/Cargo.toml")).unwrap();
        let error = update_manifests(
            &updates(a, Version::new(2, 0, 0)),
            &root.join(CARGO_TOML),
            &packages.iter().collect::<Vec<_>>(),
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("local_dependencies_update=false")
        );
        assert_eq!(fs_err::read(root.join("a/Cargo.toml")).unwrap(), before);
    }

    #[test]
    fn inherited_requirements_are_validated_and_preserved() {
        let (temp, _) = fixture();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let workspace = root.join(CARGO_TOML);
        let original = fs_err::read_to_string(&workspace).unwrap()
            + "[workspace.dependencies]\nrenamed={package=\"a\",path=\"a\",version=\"1.0.0\"}\n";
        fs_err::write(&workspace, &original).unwrap();
        let b_manifest = root.join("b/Cargo.toml");
        let b = fs_err::read_to_string(&b_manifest).unwrap().replace(
            "renamed={package=\"a\",path=\"../a\",version=\"1.0.0\"}",
            "renamed.workspace=true",
        );
        fs_err::write(&b_manifest, b).unwrap();
        let metadata = cargo_utils::get_manifest_metadata(&workspace).unwrap();
        let packages = cargo_utils::workspace_members(&metadata)
            .unwrap()
            .collect::<Vec<_>>();
        let a = packages.iter().find(|package| package.name == "a").unwrap();
        assert!(
            update_manifests(
                &updates(a, Version::new(2, 0, 0)),
                &workspace,
                &packages.iter().collect::<Vec<_>>(),
                false
            )
            .is_err()
        );
        update_manifests(
            &updates(a, Version::new(1, 0, 1)),
            &workspace,
            &packages.iter().collect::<Vec<_>>(),
            false,
        )
        .unwrap();
        assert_eq!(fs_err::read_to_string(workspace).unwrap(), original);
    }

    #[test]
    fn path_only_dependencies_allow_prerelease_versions() {
        let (temp, _) = fixture();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let b_manifest = root.join("b/Cargo.toml");
        let b = fs_err::read_to_string(&b_manifest)
            .unwrap()
            .replace(",version=\"1.0.0\"", "");
        fs_err::write(&b_manifest, b).unwrap();
        let metadata = cargo_utils::get_manifest_metadata(&root.join(CARGO_TOML)).unwrap();
        let packages = cargo_utils::workspace_members(&metadata)
            .unwrap()
            .collect::<Vec<_>>();
        let a = packages.iter().find(|package| package.name == "a").unwrap();
        update_manifests(
            &updates(a, "1.0.1-alpha.1".parse().unwrap()),
            &root.join(CARGO_TOML),
            &packages.iter().collect::<Vec<_>>(),
            false,
        )
        .unwrap();
        update_cargo_lock(root, false).unwrap();
    }

    #[test]
    fn set_version_validates_all_changes_before_writing() {
        use crate::set_version::{SetVersionRequest, SetVersionSpec, VersionChange, set_version};
        let (temp, _) = fixture();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let metadata = cargo_utils::get_manifest_metadata(&root.join(CARGO_TOML)).unwrap();
        let before = fs_err::read(root.join("a/Cargo.toml")).unwrap();
        let changes = SetVersionSpec::Workspace(
            [
                ("a".to_string(), VersionChange::new(Version::new(2, 0, 0))),
                ("b".to_string(), VersionChange::new(Version::new(2, 0, 0))),
            ]
            .into(),
        );
        let mut request = SetVersionRequest::new(changes, metadata).unwrap();
        request.set_local_dependencies_update(false);
        assert!(
            set_version(&request)
                .unwrap_err()
                .to_string()
                .contains("local_dependencies_update=false")
        );
        assert_eq!(fs_err::read(root.join("a/Cargo.toml")).unwrap(), before);
    }
}
