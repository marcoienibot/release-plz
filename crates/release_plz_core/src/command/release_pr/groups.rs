use super::*;
use cargo_utils::LocalManifest;

pub(super) const GROUP_MARKER: &str = "<!-- release-plz-per-package -->";

pub(super) async fn validate_mode(
    client: &GitClient,
    prefix: &str,
    per_package: bool,
) -> anyhow::Result<()> {
    let prs = client.opened_prs(prefix).await?;
    anyhow::ensure!(
        prs.iter().all(|pr| pr
            .body
            .as_deref()
            .is_some_and(|body| body.contains(GROUP_MARKER))
            == per_package),
        "Close or merge existing release PRs before switching pr_per_package mode"
    );
    Ok(())
}

/// Resolve existing symlinks while allowing a not-yet-created changelog suffix.
fn normalized_path(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use std::path::Component;
    let absolute = std::path::absolute(path)?;
    let mut resolved = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
        match dunce::canonicalize(&resolved) {
            Ok(path) => resolved = path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(resolved)
}

/// Build stable atomic groups from the entire eligible workspace, including unchanged members.
fn release_groups(input: &UpdateRequest) -> anyhow::Result<Vec<Vec<String>>> {
    use crate::Publishable as _;
    let packages = cargo_utils::workspace_members(input.cargo_metadata())?
        .filter(|p| p.is_publishable() && input.get_package_config(&p.name).generic.release)
        .collect::<Vec<_>>();
    let mut groups: Vec<Vec<usize>> = (0..packages.len()).map(|i| vec![i]).collect();
    let inherited = packages
        .iter()
        .map(|p| LocalManifest::try_new(&p.manifest_path).map(|m| m.version_is_inherited()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let changelogs = packages
        .iter()
        .map(|p| normalized_path(input.changelog_path(p).as_std_path()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let package_paths = packages
        .iter()
        .map(|p| normalized_path(p.manifest_path.parent().unwrap().as_std_path()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let dependency_paths = packages
        .iter()
        .map(|p| {
            p.dependencies
                .iter()
                .filter_map(|d| d.path.as_ref())
                .map(|p| normalized_path(p.as_std_path()))
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for a in 0..packages.len() {
        for b in a + 1..packages.len() {
            let pa = &packages[a];
            let pb = &packages[b];
            let ca = input.get_package_config(&pa.name);
            let cb = input.get_package_config(&pb.name);
            let linked = dependency_paths[a].contains(&package_paths[b])
                || dependency_paths[b].contains(&package_paths[a])
                || (inherited[a] && inherited[b])
                || (ca.version_group.is_some() && ca.version_group == cb.version_group)
                || changelogs[a] == changelogs[b]
                || ca.changelog_include.contains(&pb.name.to_string())
                || cb.changelog_include.contains(&pa.name.to_string());
            if linked {
                let ga = groups.iter().position(|g| g.contains(&a)).unwrap();
                let gb = groups.iter().position(|g| g.contains(&b)).unwrap();
                if ga != gb {
                    let other = groups.remove(gb);
                    let ga = groups.iter().position(|g| g.contains(&a)).unwrap();
                    groups[ga].extend(other);
                }
            }
        }
    }
    let mut groups = groups
        .into_iter()
        .map(|g| {
            let mut names = g
                .into_iter()
                .map(|i| packages[i].name.to_string())
                .collect::<Vec<_>>();
            names.sort();
            names
        })
        .collect::<Vec<_>>();
    groups.sort();
    if let Some(selected) = input.single_package() {
        anyhow::ensure!(
            groups.iter().any(|g| g.iter().any(|n| n == selected)),
            "package `{selected}` is not an eligible release package"
        );
        groups.retain(|g| g.iter().any(|n| n == selected));
    }
    Ok(groups)
}

fn group_prefix(prefix: &str, group: &[String]) -> String {
    // Lengths distinguish a from a-b and keep identities stable when only one group member changes.
    format!(
        "{prefix}package-{}-",
        group
            .iter()
            .map(|n| format!("{}_{n}", n.len()))
            .collect::<Vec<_>>()
            .join("+")
    )
}

pub(super) async fn release_pr_per_package(
    input: &ReleasePrRequest,
    root: &Utf8Path,
    prefix: &str,
) -> anyhow::Result<Option<Vec<ReleasePr>>> {
    let groups = release_groups(&input.update_request)?;
    validate_labels(&input.labels)?;
    let client = input
        .update_request
        .git_client()?
        .context("can't find git client")?;
    validate_mode(&client, prefix, true).await?;
    let manifest_dir = input.update_request.local_manifest_dir()?;
    let plan_dir = copy_to_temp_dir(root)?;
    let plan_manifest =
        new_manifest_dir_path(root, manifest_dir, plan_dir.path())?.join(CARGO_TOML);
    let mut plan_request = input
        .update_request
        .clone()
        .without_single_package()
        .set_local_manifest(&plan_manifest)?;
    if input.update_request.single_package().is_some() {
        let names = plan_request
            .cargo_metadata()
            .packages
            .iter()
            .map(|p| p.name.to_string())
            .collect::<Vec<_>>();
        for name in names {
            if !groups.iter().flatten().any(|member| member == &name) {
                let mut config = plan_request.get_package_config(&name);
                config.generic.release = false;
                plan_request = plan_request.with_package_config(name, config);
            }
        }
    }
    let (plan, _repository) = crate::next_versions(&plan_request).await?;
    let mut prs = vec![];
    for group in groups {
        let updates = plan
            .updates()
            .iter()
            .filter(|(p, _)| group.contains(&p.name.to_string()))
            .collect::<Vec<_>>();
        if updates.is_empty() {
            continue;
        }
        let directory = copy_to_temp_dir(root)?;
        let local_manifest =
            new_manifest_dir_path(root, manifest_dir, directory.path())?.join(CARGO_TOML);
        let request = input
            .update_request
            .clone()
            .set_local_manifest(&local_manifest)?;
        let metadata = cargo_utils::get_manifest_metadata(&local_manifest)?;
        let mut selected = PackagesUpdate::new(
            updates
                .into_iter()
                .map(|(p, u)| {
                    let local = metadata
                        .packages
                        .iter()
                        .find(|local| local.name == p.name)
                        .context("planned package missing from copied workspace")?;
                    Ok((local.clone(), u.clone()))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
        if let Some(version) = plan.workspace_version()
            && selected.updates().iter().any(|(p, _)| {
                LocalManifest::try_new(&p.manifest_path).is_ok_and(|m| m.version_is_inherited())
            })
        {
            selected.with_workspace_version(version.clone());
        }
        super::super::update::apply_updates(&request, &selected)?;
        let copied_root = new_project_root(root, directory.path())?;
        let repo = Repo::new(&copied_root)?;
        if repo.is_clean().is_ok() {
            continue;
        }
        let pr = open_or_update_release_pr(
            &local_manifest,
            &selected,
            &client,
            &repo,
            ReleasePrOptions {
                draft: input.draft,
                pr_name: input.pr_name_template.clone(),
                pr_body: input.pr_body_template.clone(),
                pr_labels: input.labels.clone(),
                pr_branch_prefix: group_prefix(prefix, &group),
                prefix_template: Some(input.branch_prefix.clone()),
                per_package: true,
            },
        )
        .await?;
        prs.push(pr);
    }
    Ok((!prs.is_empty()).then_some(prs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, UpdateRequest) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        fs_err::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"a\",\"a-b\",\"MixedCase\"]\nresolver=\"3\"\n",
        )
        .unwrap();
        for name in ["a", "a-b", "MixedCase"] {
            fs_err::create_dir_all(root.join(name).join("src")).unwrap();
            fs_err::write(root.join(name).join("src/lib.rs"), "").unwrap();
            fs_err::write(
                root.join(name).join("Cargo.toml"),
                format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\nedition=\"2024\"\n"),
            )
            .unwrap();
        }
        let metadata = cargo_utils::get_manifest_metadata(&root.join("Cargo.toml")).unwrap();
        let request = UpdateRequest::new(metadata).unwrap();
        (dir, request)
    }

    #[tokio::test]
    async fn atomic_groups_open_isolated_prs_and_reuse_them_on_rerun() {
        use std::sync::{Arc, Mutex};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::any};
        let (dir, request) = workspace();
        let repo = Repo::init(dir.path());
        fs_err::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        repo.add_all_and_commit("initial workspace").unwrap();
        let remote = tempfile::tempdir().unwrap();
        repo.git(&["init", "--bare", remote.path().to_str().unwrap()])
            .unwrap();
        repo.git(&["remote", "add", "origin", remote.path().to_str().unwrap()])
            .unwrap();
        repo.git(&["push", "--set-upstream", "origin", repo.original_branch()])
            .unwrap();
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let responses = state.clone();
        Mock::given(any()).respond_with(move |req: &Request| {
            let mut prs = responses.lock().unwrap();
            if req.method == "GET" && req.url.path().ends_with("/commits") {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!([{"sha":"unused", "author":{"id":1,"login":"bot"}}]));
            }
            if req.method == "GET" { return ResponseTemplate::new(200).set_body_json(&*prs); }
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if req.method == "POST" {
                let number = prs.len()+1;
                let pr = serde_json::json!({"number":number,"user":{"id":1,"login":"bot"},"html_url":format!("https://example.com/pr/{number}"),"head":{"ref":body["head"],"sha":"unused"},"title":body["title"],"body":body["body"],"labels":[]});
                prs.push(pr.clone()); return ResponseTemplate::new(201).set_body_json(pr);
            }
            assert!(body.get("state").is_none(), "must not close sibling PRs");
            ResponseTemplate::new(200)
        }).mount(&server).await;
        let github = crate::GitHub::new(
            "owner".into(),
            "repo".into(),
            secrecy::SecretString::from("token"),
        )
        .with_base_url(server.uri().parse().unwrap());
        let forge = crate::GitForge::Gitea(crate::git::gitea_client::Gitea {
            remote: github.remote,
        });
        let config = crate::UpdateConfig {
            git_only: Some(true),
            publish: false,
            semver_check: false,
            ..Default::default()
        };
        let disabled = crate::UpdateConfig {
            release: false,
            ..config.clone()
        };
        let request = request
            .with_default_package_config(config)
            .with_package_config("MixedCase", disabled.into())
            .with_git_client(forge)
            .with_repo_url(crate::RepoUrl::new("https://github.com/owner/repo").unwrap());
        let input = ReleasePrRequest::new(request)
            .with_pr_per_package(Some(true))
            .with_branch_prefix(Some("release-{{ branch }}-".into()));
        let first = release_pr(&input).await.unwrap().unwrap();
        assert_eq!(first.len(), 2);
        for pr in &first {
            assert_eq!(pr.releases.len(), 1);
            let name = &pr.releases[0].package_name;
            let other = if name == "a" { "a-b" } else { "a" };
            let reference = format!("{}:{name}/CHANGELOG.md", pr.head_branch);
            let content = repo
                .git(&[
                    "--git-dir",
                    remote.path().to_str().unwrap(),
                    "show",
                    &reference,
                ])
                .unwrap();
            assert!(content.contains("0.1.0"));
            let other_reference = format!("{}:{other}/CHANGELOG.md", pr.head_branch);
            assert!(
                repo.git(&[
                    "--git-dir",
                    remote.path().to_str().unwrap(),
                    "show",
                    &other_reference
                ])
                .is_err()
            );
        }
        let again = release_pr(&input).await.unwrap().unwrap();
        assert_eq!(
            first.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            again.iter().map(|pr| pr.number).collect::<Vec<_>>()
        );
        assert_eq!(state.lock().unwrap().len(), 2);
        let selected =
            ReleasePrRequest::new(input.update_request.clone().with_single_package("a".into()))
                .with_pr_per_package(Some(true))
                .with_branch_prefix(Some(input.branch_prefix.clone()));
        let selected_prs = release_pr(&selected).await.unwrap().unwrap();
        assert_eq!(selected_prs.len(), 1);
        assert_eq!(selected_prs[0].releases[0].package_name, "a");
        assert_eq!(state.lock().unwrap().len(), 2);

        let client = input.update_request.git_client().unwrap().unwrap();
        assert!(
            validate_mode(
                &client,
                &format!("release-{}-", repo.original_branch()),
                false
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn atomic_groups_apply_updates_in_new_changelog_directory() {
        let (dir, request) = workspace();
        let _repo = Repo::init(dir.path());
        let package = request
            .cargo_metadata()
            .packages
            .iter()
            .find(|p| p.name == "a")
            .unwrap()
            .clone();
        let config = crate::UpdateConfig {
            changelog_path: Some("docs/new/CHANGELOG.md".into()),
            ..Default::default()
        };
        let request = request.with_default_package_config(config);
        let update = crate::UpdateResult {
            version: "0.1.1".parse().unwrap(),
            changelog: Some("# Changelog\n\n## 0.1.1\n".into()),
            semver_check: crate::semver_check::SemverCheck::Skipped,
            new_changelog_entry: None,
            registry_version: None,
        };
        super::super::super::update::apply_updates(
            &request,
            &PackagesUpdate::new(vec![(package, update)]),
        )
        .unwrap();
        assert!(
            fs_err::read_to_string(dir.path().join("docs/new/CHANGELOG.md"))
                .unwrap()
                .contains("0.1.1")
        );
    }

    #[test]
    fn atomic_groups_allow_new_changelog_directories_and_ignore_registry_namesakes() {
        let (dir, request) = workspace();
        let config = crate::UpdateConfig {
            changelog_path: Some("docs/new/../future/CHANGELOG.md".into()),
            ..Default::default()
        };
        assert_eq!(
            release_groups(&request.with_default_package_config(config))
                .unwrap()
                .len(),
            1
        );
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let path = root.join("a-b/Cargo.toml");
        let mut content = fs_err::read_to_string(&path).unwrap();
        content.push_str("\n[dependencies]\na=\"0.1\"\n");
        fs_err::write(path, content).unwrap();
        let request = UpdateRequest::new(
            cargo_utils::get_manifest_metadata(&root.join("Cargo.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(release_groups(&request).unwrap().len(), 3);
    }

    #[test]
    #[cfg(unix)]
    fn atomic_groups_resolve_changelog_file_symlinks() {
        let (dir, request) = workspace();
        fs_err::write(dir.path().join("real.md"), "").unwrap();
        std::os::unix::fs::symlink("real.md", dir.path().join("alias.md")).unwrap();
        let a = crate::UpdateConfig {
            changelog_path: Some("real.md".into()),
            ..Default::default()
        };
        let b = crate::UpdateConfig {
            changelog_path: Some("alias.md".into()),
            ..Default::default()
        };
        let request = request
            .with_package_config("a", a.into())
            .with_package_config("a-b", b.into())
            .with_single_package("a".into());
        assert_eq!(release_groups(&request).unwrap(), vec![vec!["a", "a-b"]]);
    }

    #[test]
    fn atomic_groups_include_dependencies_and_inherited_versions() {
        let (dir, _) = workspace();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let manifest = root.join("a-b/Cargo.toml");
        let mut content = fs_err::read_to_string(&manifest).unwrap();
        content.push_str("\n[dependencies]\na={version=\"0.1.0\",path=\"../a\"}\n");
        fs_err::write(&manifest, content).unwrap();
        let req = UpdateRequest::new(
            cargo_utils::get_manifest_metadata(&root.join("Cargo.toml")).unwrap(),
        )
        .unwrap()
        .with_single_package("a".into());
        assert_eq!(release_groups(&req).unwrap(), vec![vec!["a", "a-b"]]);
        let (dir, _) = workspace();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let mut manifest = fs_err::read_to_string(root.join("Cargo.toml")).unwrap();
        manifest.push_str("\n[workspace.package]\nversion=\"0.1.0\"\n");
        fs_err::write(root.join("Cargo.toml"), manifest).unwrap();
        for name in ["a", "a-b"] {
            let path = root.join(name).join("Cargo.toml");
            let text = fs_err::read_to_string(&path)
                .unwrap()
                .replace("version=\"0.1.0\"", "version.workspace=true");
            fs_err::write(path, text).unwrap();
        }
        let req = UpdateRequest::new(
            cargo_utils::get_manifest_metadata(&root.join("Cargo.toml")).unwrap(),
        )
        .unwrap()
        .with_single_package("a-b".into());
        assert_eq!(release_groups(&req).unwrap(), vec![vec!["a", "a-b"]]);
    }

    #[test]
    fn atomic_groups_preserve_case_selection_and_disable_overrides() {
        let (_dir, request) = workspace();
        let groups = release_groups(&request).unwrap();
        assert_eq!(groups, vec![vec!["MixedCase"], vec!["a"], vec!["a-b"]]);
        assert!(
            !group_prefix("release-", &["a-b".into()])
                .starts_with(&group_prefix("release-", &["a".into()]))
        );
        let selected = request.clone().with_single_package("MixedCase".into());
        assert_eq!(release_groups(&selected).unwrap(), vec![vec!["MixedCase"]]);
        let disabled = crate::UpdateConfig {
            release: false,
            ..Default::default()
        };
        let request = request.with_package_config("a", disabled.into());
        assert!(
            !release_groups(&request)
                .unwrap()
                .iter()
                .flatten()
                .any(|n| n == "a")
        );
    }

    #[test]
    fn atomic_groups_keep_version_groups_and_shared_changelogs_together() {
        let (_dir, request) = workspace();
        let config = crate::PackageUpdateConfig {
            version_group: Some("atomic".into()),
            ..Default::default()
        };
        let request = request
            .with_package_config("a", config.clone())
            .with_package_config("a-b", config);
        let selected = request.with_single_package("a".into());
        assert_eq!(release_groups(&selected).unwrap(), vec![vec!["a", "a-b"]]);
        let (_dir, request) = workspace();
        let config = crate::UpdateConfig {
            changelog_path: Some("CHANGELOG.md".into()),
            ..Default::default()
        };
        let request = request.with_default_package_config(config);
        assert_eq!(
            release_groups(&request).unwrap(),
            vec![vec!["MixedCase", "a", "a-b"]]
        );
    }
}
