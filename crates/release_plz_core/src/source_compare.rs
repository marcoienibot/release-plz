//! Comparison for explicitly reconstructed git-only source baselines.
//! Cargo remains responsible for selecting exported files, including include/exclude
//! rules, nested package boundaries and external readme/license files.
use std::collections::{BTreeMap, HashSet};

use anyhow::Context as _;
use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};

use crate::cargo::run_cargo;

struct SourceFiles {
    root: Utf8PathBuf,
    files: BTreeMap<Utf8PathBuf, Utf8PathBuf>,
    gitlinks: BTreeMap<Utf8PathBuf, git2::Oid>,
}

pub(crate) fn are_packages_equal(
    name: &str,
    local: &Utf8Path,
    baseline: &Utf8Path,
) -> anyhow::Result<bool> {
    // Historical commits may predate the manifest, or contain different manifest
    // contents. Do not require their metadata to resolve before reporting a change.
    if fs_err::read(local.join("Cargo.toml")).ok() != fs_err::read(baseline.join("Cargo.toml")).ok()
    {
        return Ok(false);
    }
    let mut local_files = source_files(name, local)?;
    let mut baseline_files = source_files(name, baseline)?;

    // A superproject checkout changes gitlinks, but does not check out the
    // submodule's working tree. Compare the recorded commits for submodules whose
    // contents Cargo selects, instead of hashing stale or absent checkout contents.
    let submodules: HashSet<_> = local_files
        .gitlinks
        .keys()
        .chain(baseline_files.gitlinks.keys())
        .cloned()
        .collect();
    for submodule in submodules {
        let selected = local_files
            .files
            .keys()
            .chain(baseline_files.files.keys())
            .any(|file| file.starts_with(&submodule));
        if selected {
            if local_files.gitlinks.get(&submodule) != baseline_files.gitlinks.get(&submodule) {
                return Ok(false);
            }
            local_files
                .files
                .retain(|file, _| !file.starts_with(&submodule));
            baseline_files
                .files
                .retain(|file, _| !file.starts_with(&submodule));
        }
    }
    if !local_files.files.keys().eq(baseline_files.files.keys()) {
        return Ok(false);
    }
    for (name, local_file) in local_files.files {
        let baseline_file = &baseline_files.files[&name];
        // Read through symlinks, as Cargo does when exporting them. Missing or
        // unreadable selected files are errors, never silently equal.
        for (path, root) in [
            (&local_file, &local_files.root),
            (baseline_file, &baseline_files.root),
        ] {
            let resolved = crate::fs_utils::canonicalize_utf8(path)?;
            anyhow::ensure!(
                resolved.starts_with(root),
                "selected source file {path} resolves outside repository {root}; its historical contents cannot be reconstructed from git"
            );
        }
        let local_content = fs_err::read(&local_file)
            .with_context(|| format!("read selected source file {local_file}"))?;
        let baseline_content = fs_err::read(baseline_file)
            .with_context(|| format!("read selected baseline file {baseline_file}"))?;
        if local_content != baseline_content {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Repository-relative source paths for commit attribution. Include both a
/// symlink and its target, external README/license paths, and selected gitlinks.
pub(crate) fn package_paths(
    name: &str,
    package: &Utf8Path,
) -> anyhow::Result<HashSet<Utf8PathBuf>> {
    let selected = source_files(name, package)?;
    let mut paths = HashSet::new();
    for path in selected.files.values() {
        if let Ok(relative) = path.strip_prefix(&selected.root) {
            paths.insert(relative.to_owned());
        }
        if let Ok(canonical) = crate::fs_utils::canonicalize_utf8(path)
            && let Ok(relative) = canonical.strip_prefix(&selected.root)
        {
            paths.insert(relative.to_owned());
        }
    }
    for submodule in selected.gitlinks.keys() {
        if selected
            .files
            .keys()
            .any(|file| file.starts_with(submodule))
        {
            let path = package.join(submodule);
            paths.insert(path.strip_prefix(&selected.root)?.to_owned());
        }
    }
    Ok(paths)
}

fn source_files(name: &str, package: &Utf8Path) -> anyhow::Result<SourceFiles> {
    let metadata = cargo_utils::get_manifest_metadata(&package.join("Cargo.toml"))?;
    let details = metadata
        .workspace_packages()
        .into_iter()
        .find(|candidate| candidate.name == name)
        .context("find source package in metadata")?;
    // --list succeeds for versionless path dependencies even though producing a
    // publishable archive does not. Avoid get_cargo_package_files' archive heuristics.
    let output = run_cargo(package, &["package", "--list", "--quiet", "--allow-dirty"])?;
    anyhow::ensure!(
        output.status.success(),
        "list source package files: {}",
        output.stderr
    );
    let repo = git2::Repository::discover(package)?;
    let root = crate::fs_utils::to_utf8_path(
        repo.workdir()
            .context("source repository has no working directory")?,
    )?
    .to_owned();
    let package_relative = package.strip_prefix(&root)?;
    let mut gitlinks = BTreeMap::new();
    for entry in repo.index()?.iter().filter(|entry| entry.mode == 0o160_000) {
        let path = Utf8Path::new(std::str::from_utf8(&entry.path)?);
        if let Ok(relative) = path.strip_prefix(package_relative) {
            gitlinks.insert(relative.to_owned(), entry.id);
        }
    }
    let mut files = BTreeMap::new();
    for file in output.stdout.lines().map(Utf8PathBuf::from) {
        if matches!(
            file.as_str(),
            "Cargo.lock" | "Cargo.toml.orig" | ".cargo_vcs_info.json"
        ) {
            continue;
        }
        let mut path = package.join(&file);
        for special in [details.readme.as_ref(), details.license_file.as_ref()]
            .into_iter()
            .flatten()
        {
            let original = package.join(special);
            let canonical = crate::fs_utils::canonicalize_utf8(&original)?;
            if !canonical.starts_with(package) && special.file_name() == Some(file.as_str()) {
                path = crate::fs_utils::canonicalize_utf8(
                    original.parent().context("source file has no parent")?,
                )?
                .join(original.file_name().context("source file has no name")?);
            }
        }
        files.insert(file, path);
    }
    Ok(SourceFiles {
        root,
        files,
        gitlinks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sources {
        _temp: tempfile::TempDir,
        local: Utf8PathBuf,
        baseline: Utf8PathBuf,
    }

    impl Sources {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = crate::fs_utils::to_utf8_path(temp.path()).unwrap();
            let local = root.join("local/app");
            let baseline = root.join("baseline/app");
            for app in [&local, &baseline] {
                let root = app.parent().unwrap();
                fs_err::create_dir_all(app.join("src")).unwrap();
                fs_err::create_dir_all(root.join("internal/src")).unwrap();
                fs_err::write(
                    root.join("Cargo.toml"),
                    "[workspace]\nmembers=[\"app\",\"internal\"]\nresolver=\"3\"\n",
                )
                .unwrap();
                fs_err::write(app.join("Cargo.toml"), "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\nreadme=\"../INFO.txt\"\nlicense-file=\"../LEGAL.txt\"\nexclude=[\"ignored.txt\"]\n[dependencies]\ninternal={path=\"../internal\"}\n").unwrap();
                fs_err::write(
                    root.join("internal/Cargo.toml"),
                    "[package]\nname=\"internal\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
                )
                .unwrap();
                fs_err::write(root.join("internal/src/lib.rs"), "").unwrap();
                fs_err::write(app.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }").unwrap();
                fs_err::write(root.join("INFO.txt"), "readme").unwrap();
                fs_err::write(root.join("LEGAL.txt"), "license").unwrap();
                git2::Repository::init(root).unwrap();
                commit(root);
            }
            Self {
                _temp: temp,
                local,
                baseline,
            }
        }
        fn equal(&self) -> bool {
            are_packages_equal("app", &self.local, &self.baseline).unwrap()
        }
    }

    fn commit(root: &Utf8Path) {
        let repo = git2::Repository::open(root).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("test", "test@example.com").unwrap();
        let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "fixture",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap();
    }

    #[test]
    fn source_selection_handles_versionless_dependencies_and_package_boundaries() {
        let fixture = Sources::new();
        assert!(fixture.equal());
        for app in [&fixture.local, &fixture.baseline] {
            fs_err::write(app.join("ignored.txt"), app.as_str()).unwrap();
            fs_err::create_dir_all(app.join("nested/src")).unwrap();
            fs_err::write(
                app.join("nested/Cargo.toml"),
                "[package]\nname=\"nested\"\nversion=\"0.1.0\"\n[workspace]\n",
            )
            .unwrap();
            fs_err::write(app.join("nested/src/lib.rs"), app.as_str()).unwrap();
            commit(app.parent().unwrap());
        }
        assert!(
            fixture.equal(),
            "excluded files and nested packages must not change app"
        );
        assert!(
            !package_paths("app", &fixture.local)
                .unwrap()
                .contains(Utf8Path::new("app/ignored.txt"))
        );
        fs_err::write(
            fixture.local.join("src/lib.rs"),
            "pub fn answer() -> u8 { 43 }",
        )
        .unwrap();
        assert!(!fixture.equal());
    }

    #[test]
    fn external_readme_and_license_contents_and_paths_are_compared() {
        let fixture = Sources::new();
        let paths = package_paths("app", &fixture.local).unwrap();
        assert!(paths.contains(Utf8Path::new("INFO.txt")));
        assert!(paths.contains(Utf8Path::new("LEGAL.txt")));
        fs_err::write(
            fixture.local.parent().unwrap().join("INFO.txt"),
            "new readme",
        )
        .unwrap();
        assert!(!fixture.equal());
        fs_err::write(fixture.local.parent().unwrap().join("INFO.txt"), "readme").unwrap();
        fs_err::write(
            fixture.local.parent().unwrap().join("LEGAL.txt"),
            "new license",
        )
        .unwrap();
        assert!(!fixture.equal());
    }

    #[cfg(unix)]
    #[test]
    fn exported_symlink_contents_are_compared_and_link_paths_are_attributed() {
        let fixture = Sources::new();
        for app in [&fixture.local, &fixture.baseline] {
            fs_err::write(
                app.parent().unwrap().join("shared.rs"),
                "pub fn linked() {}",
            )
            .unwrap();
            std::os::unix::fs::symlink("../../shared.rs", app.join("src/linked.rs")).unwrap();
            commit(app.parent().unwrap());
        }
        assert!(fixture.equal());
        let paths = package_paths("app", &fixture.local).unwrap();
        assert!(paths.contains(Utf8Path::new("app/src/linked.rs")));
        assert!(paths.contains(Utf8Path::new("shared.rs")));
        fs_err::write(
            fixture.local.parent().unwrap().join("shared.rs"),
            "pub fn changed() {}",
        )
        .unwrap();
        assert!(!fixture.equal());
    }

    #[cfg(unix)]
    #[test]
    fn external_readme_symlink_preserves_alias_and_target_paths() {
        let fixture = Sources::new();
        for app in [&fixture.local, &fixture.baseline] {
            let root = app.parent().unwrap();
            fs_err::remove_file(root.join("INFO.txt")).unwrap();
            fs_err::write(root.join("readme-target.txt"), "readme").unwrap();
            std::os::unix::fs::symlink("readme-target.txt", root.join("INFO.txt")).unwrap();
            commit(root);
        }
        assert!(fixture.equal());
        let paths = package_paths("app", &fixture.local).unwrap();
        assert!(paths.contains(Utf8Path::new("INFO.txt")));
        assert!(paths.contains(Utf8Path::new("readme-target.txt")));
        fs_err::write(
            fixture.local.parent().unwrap().join("readme-target.txt"),
            "changed",
        )
        .unwrap();
        assert!(!fixture.equal());
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_outside_the_repository_are_reported_instead_of_ignored() {
        let fixture = Sources::new();
        let root = fixture.local.parent().unwrap().parent().unwrap();
        fs_err::write(root.join("external.rs"), "pub fn external() {}").unwrap();
        for app in [&fixture.local, &fixture.baseline] {
            std::os::unix::fs::symlink("../../../external.rs", app.join("src/external.rs"))
                .unwrap();
            commit(app.parent().unwrap());
        }
        let error = are_packages_equal("app", &fixture.local, &fixture.baseline).unwrap_err();
        assert!(error.to_string().contains("outside repository"));
    }

    #[test]
    fn selected_gitlinks_compare_commits_without_hashing_directories() {
        let fixture = Sources::new();
        let submodule_source = fixture
            .local
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("submodule-source");
        git2::Repository::init(&submodule_source).unwrap();
        fs_err::write(submodule_source.join("data.txt"), "exported").unwrap();
        commit(&submodule_source);
        for app in [&fixture.local, &fixture.baseline] {
            let status = std::process::Command::new("git")
                .current_dir(app.parent().unwrap())
                .args([
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    submodule_source.as_str(),
                    "app/vendor",
                ])
                .status()
                .unwrap();
            assert!(status.success());
            commit(app.parent().unwrap());
        }
        // Copy the same gitlink id to both superproject indexes, representing
        // equal historical submodule revisions regardless of checkout presence.
        let local_repo = git2::Repository::open(fixture.local.parent().unwrap()).unwrap();
        let baseline_repo = git2::Repository::open(fixture.baseline.parent().unwrap()).unwrap();
        let mut entry = local_repo
            .index()
            .unwrap()
            .get_path(std::path::Path::new("app/vendor"), 0)
            .unwrap();
        assert_eq!(entry.mode, 0o160_000);
        let mut index = baseline_repo.index().unwrap();
        index.add(&entry).unwrap();
        index.write().unwrap();
        assert!(fixture.equal());
        let paths = package_paths("app", &fixture.local).unwrap();
        assert!(paths.contains(Utf8Path::new("app/vendor")));
        entry.id = git2::Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        index.add(&entry).unwrap();
        index.write().unwrap();
        assert!(!fixture.equal());
    }

    #[test]
    fn archive_without_orig_keeps_registry_comparison_semantics() {
        let fixture = Sources::new();
        assert!(!crate::are_packages_equal(&fixture.local, &fixture.baseline).unwrap());
        assert!(fixture.equal());
    }
}
