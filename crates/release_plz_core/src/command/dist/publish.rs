use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, bail, ensure};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{GitHub, RepoUrl, http_client::http_client_builder, response_ext::ResponseExt as _};

use super::{
    CHECKSUMS_NAME, MANIFEST_NAME, Manifest, NOTES_NAME, Plan, TargetManifest, digest, installer,
    write_json,
};

const NOTES_START: &str = "<!-- release-plz-dist -->";
const NOTES_END: &str = "<!-- /release-plz-dist -->";

/// Locally validated files ready for upload. Preparation never contacts GitHub.
#[derive(Debug, Serialize)]
pub struct PreparedDist {
    pub(super) manifest: Manifest,
    pub(super) directory: PathBuf,
    pub(super) files: Vec<String>,
    pub(super) download_notes: String,
}

/// Require exactly the planned archives from every target and verify their
/// digests before generating installers, the manifest, checksums and release notes.
pub fn prepare(
    plan: &Plan,
    directory: &Path,
    repository: &RepoUrl,
) -> anyhow::Result<PreparedDist> {
    let mut artifacts = BTreeMap::new();
    for target in &plan.targets {
        let manifest_path = directory.join(plan.target_manifest_name(target));
        let target_manifest: TargetManifest =
            serde_json::from_slice(&fs_err::read(&manifest_path).with_context(|| {
                format!(
                    "missing build manifest for {target}; collect every target before publishing"
                )
            })?)?;
        ensure!(
            target_manifest.plan == plan.for_target(target) && target_manifest.target == *target,
            "build manifest for {target} does not match this release plan (tag, commit, version, binaries and target must agree)"
        );
        let expected = plan.archives(target);
        ensure!(
            target_manifest.artifacts.len() == expected.len()
                && expected
                    .iter()
                    .all(|name| target_manifest.artifacts.contains_key(name)),
            "unexpected archive set for {target}"
        );
        for (name, artifact) in target_manifest.artifacts {
            ensure!(artifact.target == *target, "wrong target for {name}");
            // Names come from the plan, never arbitrary paths in a manifest.
            let path = directory.join(&name);
            #[expect(
                clippy::filetype_is_file,
                reason = "only regular archives may be uploaded"
            )]
            let regular_file = fs_err::symlink_metadata(&path)?.file_type().is_file();
            ensure!(regular_file, "archive must be a regular file: {name}");
            let (sha256, size) = digest(&path)?;
            ensure!(
                sha256 == artifact.sha256 && size == artifact.size,
                "checksum or size mismatch for {name}"
            );
            artifacts.insert(name, artifact);
        }
    }
    let base = repository.full_host().parse()?;
    let installers = installer::generate(plan, &artifacts, directory, &base)?;
    let manifest = Manifest {
        plan: plan.clone(),
        artifacts,
        installers,
    };
    let mut files = Vec::new();
    let mut checksums = String::new();
    for (name, sha256) in manifest
        .artifacts
        .iter()
        .map(|(name, a)| (name, &a.sha256))
        .chain(
            manifest
                .installers
                .iter()
                .map(|(name, i)| (name, &i.sha256)),
        )
    {
        let checksum_name = format!("{name}.sha256");
        let checksum = format!("{sha256}  {name}\n");
        fs_err::write(directory.join(&checksum_name), &checksum)?;
        checksums.push_str(&checksum);
        files.extend([name.clone(), checksum_name]);
    }
    write_json(&directory.join(MANIFEST_NAME), &manifest)?;
    let (manifest_digest, _) = digest(&directory.join(MANIFEST_NAME))?;
    checksums.push_str(&format!("{manifest_digest}  {MANIFEST_NAME}\n"));
    fs_err::write(directory.join(CHECKSUMS_NAME), checksums)?;
    let download_notes = download_notes(&manifest, repository)?;
    fs_err::write(directory.join(NOTES_NAME), &download_notes)?;
    files.extend([MANIFEST_NAME.to_owned(), CHECKSUMS_NAME.to_owned()]);
    files.sort();
    Ok(PreparedDist {
        manifest,
        directory: directory.to_owned(),
        files,
        download_notes,
    })
}

fn download_notes(manifest: &Manifest, repository: &RepoUrl) -> anyhow::Result<String> {
    let mut notes = format!("{NOTES_START}\n");
    let base: Url = repository.full_host().parse()?;
    if !manifest.installers.is_empty() {
        notes.push_str("## Install\n\nInstall all package binaries into `~/.local/bin` (override with `RELEASE_PLZ_INSTALL_DIR`). The installer verifies the archive's SHA-256. Add the directory to `PATH` if needed.\n\n");
        for name in manifest.installers.keys() {
            let url = download_url(&base, &manifest.plan.tag, name)?.to_string();
            let checksum_url = download_url(&base, &manifest.plan.tag, &format!("{name}.sha256"))?;
            if name.ends_with(".sh") {
                notes.push_str(&format!("Linux / macOS / FreeBSD:\n\n```sh\ncurl --proto '=https' --tlsv1.2 -LsSf {} | sh\n```\n\n", installer::shell_quote(&url)));
            } else {
                notes.push_str(&format!(
                    "Windows (PowerShell):\n\n```powershell\nirm {} | iex\n```\n\n",
                    installer::powershell_quote(&url)
                ));
            }
            notes.push_str(&format!(
                "[Installer source]({url}) · [SHA-256]({checksum_url})\n\n"
            ));
        }
    }
    notes.push_str("## Downloads\n\n| Target | Archive | SHA-256 |\n| --- | --- | --- |\n");
    for (name, artifact) in &manifest.artifacts {
        let archive_url = download_url(&base, &manifest.plan.tag, name)?;
        let checksum_url = download_url(&base, &manifest.plan.tag, &format!("{name}.sha256"))?;
        notes.push_str(&format!(
            "| `{}` | [{name}]({archive_url}) | [checksum]({checksum_url}) |\n",
            artifact.target
        ));
    }
    let checksums_url = download_url(&base, &manifest.plan.tag, CHECKSUMS_NAME)?;
    notes.push_str(&format!("\n[All checksums]({checksums_url}). Archives contain the executables at their root.\n{NOTES_END}\n"));
    Ok(notes)
}

pub(super) fn download_url(base: &Url, tag: &str, name: &str) -> anyhow::Result<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("invalid repository URL"))?
        .extend(["releases", "download", tag, name]);
    Ok(url)
}

fn release_notes(original: &str, section: &str) -> anyhow::Result<String> {
    if let Some((before, after)) = original.split_once(NOTES_START) {
        let (_, after) = after
            .split_once(NOTES_END)
            .context("existing distribution notes have no closing marker")?;
        Ok(format!("{before}{}{after}", section.trim_end()))
    } else {
        Ok(format!("{}\n\n{section}", original.trim_end()))
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    id: u64,
    tag_name: String,
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    id: u64,
    name: String,
    size: u64,
    state: String,
    digest: Option<String>,
}

/// Replace assets on an existing draft and publish it only after every upload
/// succeeds. A failed upload leaves the release draft and can be retried.
pub async fn publish(github: &GitHub, prepared: &PreparedDist) -> anyhow::Result<()> {
    let client = http_client_builder()
        .default_headers(github.default_headers()?)
        .timeout(Duration::from_secs(600))
        .build()?;
    let mut releases_url = github.remote.base_url.clone();
    releases_url
        .path_segments_mut()
        .map_err(|()| anyhow::anyhow!("invalid GitHub API URL"))?
        .pop_if_empty()
        .extend([
            "repos",
            &github.remote.owner,
            &github.remote.repo,
            "releases",
        ]);
    let tag = &prepared.manifest.plan.tag;
    // The by-tag endpoint only returns published releases. List releases to find
    // drafts too, and paginate so an older failed release can be retried.
    let release = find_release(&client, &releases_url, tag).await?;
    ensure!(
        release.draft,
        "refusing to modify already published release {tag}"
    );
    let release_url = child_url(&releases_url, &release.id.to_string());
    let assets_url = child_url(&release_url, "assets");
    let existing: Vec<ReleaseAsset> = list_pages(&client, &assets_url).await?;

    // Detect accidentally publishing a local tag to another repository/tag.
    let mut commit_url = releases_url.clone();
    commit_url
        .path_segments_mut()
        .unwrap()
        .pop()
        .extend(["commits", tag]);
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    let commit: Commit = client
        .get(commit_url)
        .send()
        .await?
        .successful_status()
        .await?
        .json()
        .await?;
    ensure!(
        commit.sha == prepared.manifest.plan.commit,
        "remote tag {tag} does not match the built commit"
    );

    // Validate notes before replacing any remote assets.
    release_notes(
        release.body.as_deref().unwrap_or_default(),
        &prepared.download_notes,
    )?;
    let upload_base: Url = release
        .upload_url
        .split('{')
        .next()
        .context("missing upload URL")?
        .parse()?;
    for name in &prepared.files {
        let path = prepared.directory.join(name);
        let (sha256, size) = digest(&path)?;
        let expected = prepared
            .manifest
            .artifacts
            .get(name)
            .map(|a| (&a.sha256, a.size))
            .or_else(|| {
                prepared
                    .manifest
                    .installers
                    .get(name)
                    .map(|i| (&i.sha256, i.size))
            });
        if let Some((expected_sha256, expected_size)) = expected {
            ensure!(
                sha256 == *expected_sha256 && size == expected_size,
                "asset changed after validation: {name}; release remains draft"
            );
        }
        for previous in existing.iter().filter(|a| a.name == *name) {
            let delete_url = child_url(
                &child_url(&releases_url, "assets"),
                &previous.id.to_string(),
            );
            client
                .delete(delete_url)
                .send()
                .await?
                .successful_status()
                .await?;
        }
        let mut upload_url = upload_base.clone();
        upload_url.query_pairs_mut().append_pair("name", name);
        tracing::info!("uploading {name}");
        let file = tokio::fs::File::open(path).await?;
        let uploaded: ReleaseAsset = client
            .post(upload_url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(reqwest::Body::from(file))
            .send()
            .await?
            .successful_status()
            .await?
            .json()
            .await?;
        ensure!(
            uploaded.name == *name && uploaded.size == size && uploaded.state == "uploaded",
            "incomplete upload for {name}; release remains draft"
        );
        if let Some(remote_digest) = uploaded.digest {
            ensure!(
                remote_digest == format!("sha256:{sha256}"),
                "GitHub checksum mismatch for {name}; release remains draft"
            );
        }
    }

    // Preserve edits to the changelog made while the uploads were running.
    let current: Release = client
        .get(release_url.clone())
        .send()
        .await?
        .successful_status()
        .await?
        .json()
        .await?;
    ensure!(
        current.draft && current.tag_name == *tag,
        "release changed while uploading; refusing to publish"
    );
    let body = release_notes(
        current.body.as_deref().unwrap_or_default(),
        &prepared.download_notes,
    )?;
    client
        .patch(release_url)
        .json(&serde_json::json!({
            "body": body,
            "draft": false,
            "make_latest": if current.prerelease { "false" } else { "true" },
        }))
        .send()
        .await?
        .successful_status()
        .await?;
    tracing::info!("published {tag}");
    Ok(())
}

fn child_url(base: &Url, component: &str) -> Url {
    let mut url = base.clone();
    url.path_segments_mut().unwrap().push(component);
    url
}

async fn find_release(client: &reqwest::Client, url: &Url, tag: &str) -> anyhow::Result<Release> {
    for page in 1.. {
        let releases: Vec<Release> = client
            .get(page_url(url, page))
            .send()
            .await?
            .successful_status()
            .await?
            .json()
            .await?;
        let count = releases.len();
        if let Some(release) = releases.into_iter().find(|r| r.tag_name == tag) {
            return Ok(release);
        }
        if count < 100 {
            break;
        }
    }
    bail!("no GitHub release for {tag}; first create a draft with release-plz release")
}

async fn list_pages<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &Url,
) -> anyhow::Result<Vec<T>> {
    let mut result = Vec::new();
    for page in 1.. {
        let values: Vec<T> = client
            .get(page_url(url, page))
            .send()
            .await?
            .successful_status()
            .await?
            .json()
            .await?;
        let count = values.len();
        result.extend(values);
        if count < 100 {
            break;
        }
    }
    Ok(result)
}

fn page_url(base: &Url, page: u32) -> Url {
    let mut url = base.clone();
    url.query_pairs_mut()
        .append_pair("per_page", "100")
        .append_pair("page", &page.to_string());
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_preserve_surrounding_text_and_are_idempotent() {
        let section = format!("{NOTES_START}\nDownloads\n{NOTES_END}\n");
        let original =
            "Changes with `code`, $variables and 'quotes'.\n\n### Contributors\n* @alice";
        let notes = release_notes(original, &section).unwrap();
        assert!(notes.starts_with(original));
        assert_eq!(release_notes(&notes, &section).unwrap(), notes);
        let notes = format!("{notes}\nHandwritten footer");
        assert_eq!(release_notes(&notes, &section).unwrap(), notes);
        assert!(release_notes(NOTES_START, &section).is_err());
    }
}
