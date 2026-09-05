//! An additive, reproducible overview of independent package histories.
use std::{
    collections::{BTreeMap, HashSet},
    sync::LazyLock,
};

use anyhow::{Context as _, ensure};
use cargo_metadata::camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use chrono::NaiveDate;
use pulldown_cmark::{Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use regex::Regex;
use url::Url;

use super::{PackagesUpdate, UpdateRequest};

const START: &str = "<!-- release-plz workspace changelog -->";
const END: &str = "<!-- /release-plz workspace changelog -->";

pub(super) fn prepare(
    request: &UpdateRequest,
    updates: &PackagesUpdate,
) -> anyhow::Result<Option<(Utf8PathBuf, String)>> {
    let Some(relative) = request.workspace_changelog_path() else {
        return Ok(None);
    };
    ensure!(
        !relative.as_str().is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Utf8Component::Normal(_) | Utf8Component::CurDir)),
        "workspace_changelog must be a relative file path without '..'"
    );
    let path = request.local_manifest().parent().unwrap().join(relative);
    let destination = resolved_path(&path)?;
    let mut histories = Vec::new();
    let mut source_paths = HashSet::new();
    // release-pr changes local_manifest to a temporary checkout but retains the
    // original metadata. Read paths in the checkout we are actually updating.
    let metadata = cargo_utils::get_manifest_metadata(request.local_manifest())?;
    for package in cargo_utils::workspace_members(&metadata)? {
        let source = request.changelog_path(&package);
        ensure!(
            resolved_path(&source)? != destination,
            "workspace_changelog must differ from the changelog of {}",
            package.name
        );
        let config = request.get_package_config(&package.name);
        if !config.generic.release || !config.should_update_changelog() {
            continue;
        }
        ensure!(
            source_paths.insert(resolved_path(&source)?),
            "workspace_changelog requires independent package changelogs; multiple packages use {source}"
        );
        let planned = updates
            .updates()
            .iter()
            .find(|(p, _)| p.name == package.name)
            .and_then(|(_, update)| update.changelog.clone());
        let text = match planned {
            Some(text) => text,
            None => match fs_err::read_to_string(&source) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| format!("cannot read {source}")),
            },
        };
        histories.push((package.name.to_string(), source, text));
    }
    histories.sort_by(|a, b| a.0.cmp(&b.0));
    let content = render(&histories, &path)?;
    let existing = match fs_err::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "# Workspace changelog\n\n".to_string()
        }
        Err(error) => return Err(error).context("cannot read workspace changelog"),
    };
    Ok(Some((path, replace_generated(&existing, &content)?)))
}

/// Resolve existing ancestors too, so a symlink alias cannot overwrite a package log.
fn resolved_path(path: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.try_exists()? {
            let resolved = cargo_utils::to_utf8_pathbuf(fs_err::canonicalize(ancestor)?)?;
            return Ok(resolved.join(path.strip_prefix(ancestor)?));
        }
    }
    anyhow::bail!("cannot resolve changelog path {path}")
}

fn render(
    histories: &[(String, Utf8PathBuf, String)],
    output: &Utf8Path,
) -> anyhow::Result<String> {
    static DATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[0-9]{4}-[0-9]{2}-[0-9]{2}\b").unwrap());
    let mut dates: BTreeMap<Option<NaiveDate>, Vec<String>> = BTreeMap::new();
    for (name, source, text) in histories {
        let changelog = parse_changelog::parse(text)
            .with_context(|| format!("cannot aggregate changelog {source}"))?;
        for release in changelog.values() {
            if release.version.eq_ignore_ascii_case("unreleased") {
                continue;
            }
            let title = release.title_no_link();
            // SemVer prerelease/build identifiers can themselves contain dates.
            let suffix = title.find(release.version).map_or(title.as_ref(), |start| {
                &title[start + release.version.len()..]
            });
            let date = DATE
                .find(suffix)
                .map(|date| NaiveDate::parse_from_str(date.as_str(), "%Y-%m-%d"))
                .transpose()
                .with_context(|| format!("invalid release date in {source}"))?;
            let notes = relocate_notes(release.notes, text, source, output)?;
            let source_url =
                Url::from_file_path(source).map_err(|()| anyhow::anyhow!("invalid source path"))?;
            let output_url =
                Url::from_file_path(output).map_err(|()| anyhow::anyhow!("invalid output path"))?;
            let source_link = output_url
                .make_relative(&source_url)
                .context("cannot link package changelog")?;
            // Each package keeps its actual version; the workspace has no invented version.
            dates.entry(date).or_default().push(format!(
                "### {name} {}\n\n[Package changelog]({source_link})\n\n{notes}\n\n",
                release.version
            ));
        }
    }
    let mut result = String::new();
    for (date, entries) in dates.into_iter().rev() {
        let date = date.map_or_else(|| "Undated releases".to_string(), |d| d.to_string());
        result.push_str(&format!("## {date}\n\n"));
        for entry in entries {
            result.push_str(&entry);
        }
    }
    Ok(result)
}

fn relative_url(destination: &str, source: &Utf8Path, output: &Utf8Path) -> anyhow::Result<String> {
    let source =
        Url::from_file_path(source).map_err(|()| anyhow::anyhow!("invalid source path"))?;
    let output =
        Url::from_file_path(output).map_err(|()| anyhow::anyhow!("invalid output path"))?;
    let url = source.join(destination)?;
    output
        .make_relative(&url)
        .context("cannot relocate changelog link")
}

fn relocate_notes(
    notes: &str,
    complete_changelog: &str,
    source: &Utf8Path,
    output: &Utf8Path,
) -> anyhow::Result<String> {
    let mut events = Vec::new();
    let definitions = Parser::new(complete_changelog);
    let resolve_link = |link: pulldown_cmark::BrokenLink<'_>| {
        definitions
            .reference_definitions()
            .get(link.reference.as_ref())
            .map(|definition| {
                (
                    definition.dest.to_string().into(),
                    definition
                        .title
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string)
                        .into(),
                )
            })
    };
    for mut event in Parser::new_with_broken_link_callback(
        notes,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
        Some(resolve_link),
    ) {
        match &mut event {
            Event::Start(Tag::Heading { level, .. }) | Event::End(TagEnd::Heading(level)) => {
                *level = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => HeadingLevel::H4,
                    HeadingLevel::H4 => HeadingLevel::H5,
                    HeadingLevel::H5 | HeadingLevel::H6 => HeadingLevel::H6,
                };
            }
            Event::Start(
                Tag::Link {
                    dest_url,
                    link_type,
                    ..
                }
                | Tag::Image {
                    dest_url,
                    link_type,
                    ..
                },
            ) if !dest_url.starts_with('/') && Url::parse(dest_url).is_err() => {
                *dest_url = relative_url(dest_url, source, output)?.into();
                *link_type = LinkType::Inline;
            }
            _ => {}
        }
        events.push(event);
    }
    let mut output = String::new();
    pulldown_cmark_to_cmark::cmark(events.into_iter(), &mut output)?;
    Ok(output)
}

fn replace_generated(existing: &str, generated: &str) -> anyhow::Result<String> {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for (event, range) in Parser::new(existing).into_offset_iter() {
        if let Event::Html(comment) = event {
            let marker = comment.trim();
            if marker == START || marker == END {
                let offset = range.start + comment.find(marker).unwrap();
                if marker == START {
                    starts.push((offset, ()));
                } else {
                    ends.push((offset, ()));
                }
            }
        }
    }
    let block = format!("{START}\n\n{generated}{END}");
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(format!("{}\n\n{block}\n", existing.trim_end())),
        ([(start, _)], [(end, _)]) if start < end => Ok(format!(
            "{}{block}{}",
            &existing[..*start],
            &existing[end + END.len()..]
        )),
        _ => anyhow::bail!("workspace changelog has malformed or duplicate generated markers"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UpdateConfig, UpdateResult, semver_check::SemverCheck};

    fn fixture() -> (tempfile::TempDir, UpdateRequest) {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        fs_err::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = ['a', 'b']\nresolver = '3'\n",
        )
        .unwrap();
        for name in ["a", "b"] {
            fs_err::create_dir_all(root.join(name).join("src")).unwrap();
            fs_err::write(
                root.join(name).join("Cargo.toml"),
                format!("[package]\nname = '{name}'\nversion = '1.0.0'\nedition = '2024'\n"),
            )
            .unwrap();
            fs_err::write(root.join(name).join("src/lib.rs"), "").unwrap();
        }
        let metadata = cargo_utils::get_manifest_metadata(&root.join("a/Cargo.toml")).unwrap();
        let request = UpdateRequest::new(metadata)
            .unwrap()
            .with_workspace_changelog("docs/WORKSPACE.md".into());
        (temp, request)
    }

    #[test]
    fn rollup_preserves_independent_history_and_rebuilds_idempotently() {
        let (_temp, request) = fixture();
        let root = request.local_manifest().parent().unwrap();
        let old_a = "# Changelog\n\n## [1.0.0] - 2026-01-01\n\n### Added\n\n- First a.\n";
        let b = "# Changelog\n\n## [9.4.0] - 2026-09-05\n\n### Fixed\n\n- Fixed b.\n";
        fs_err::write(root.join("a/CHANGELOG.md"), old_a).unwrap();
        fs_err::write(root.join("b/CHANGELOG.md"), b).unwrap();
        let new_a = format!(
            "# Changelog\n\n## [1.1.0] - 2026-09-05\n\n### Added\n\n- New a.\n\n## {}",
            old_a.split_once("## ").unwrap().1
        );
        let package = cargo_utils::workspace_members(request.cargo_metadata())
            .unwrap()
            .find(|p| p.name.as_str() == "a")
            .unwrap()
            .clone();
        let updates = PackagesUpdate::new(vec![(
            package,
            UpdateResult {
                version: "1.1.0".parse().unwrap(),
                changelog: Some(new_a.clone()),
                new_changelog_entry: Some("New a".into()),
                registry_version: None,
                semver_check: SemverCheck::Skipped,
            },
        )]);
        let (path, first) = prepare(&request, &updates).unwrap().unwrap();
        assert_eq!(path, root.join("docs/WORKSPACE.md"));
        assert!(first.contains("### a 1.1.0"));
        assert!(first.contains("### b 9.4.0"));
        assert!(first.contains("### a 1.0.0"));
        assert_eq!(first.matches("## 2026-09-05\n").count(), 1);
        assert!(first.contains("#### Added"));
        assert!(first.contains("../a/CHANGELOG.md"));
        assert_eq!(
            fs_err::read_to_string(root.join("a/CHANGELOG.md")).unwrap(),
            old_a
        );
        super::super::update_changelogs(&request, &updates).unwrap();
        fs_err::create_dir_all(path.parent().unwrap()).unwrap();
        fs_err::write(&path, &first).unwrap();
        let (_, second) = prepare(&request, &PackagesUpdate::new(vec![]))
            .unwrap()
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs_err::read_to_string(root.join("a/CHANGELOG.md")).unwrap(),
            new_a
        );
        assert_eq!(
            fs_err::read_to_string(root.join("b/CHANGELOG.md")).unwrap(),
            b
        );
    }

    #[test]
    fn rollup_respects_package_gate_and_rejects_path_collisions() {
        let (_temp, request) = fixture();
        let root = request.local_manifest().parent().unwrap();
        fs_err::write(
            root.join("a/CHANGELOG.md"),
            "# Changelog\n\n## [1.0.0]\n\n- A\n",
        )
        .unwrap();
        let disabled = request.clone().with_package_config(
            "a",
            UpdateConfig::default().with_changelog_update(false).into(),
        );
        assert!(
            !prepare(&disabled, &PackagesUpdate::new(vec![]))
                .unwrap()
                .unwrap()
                .1
                .contains("### a")
        );
        for path in ["a/CHANGELOG.md", "a/./CHANGELOG.md", "../CHANGELOG.md"] {
            assert!(
                prepare(
                    &request.clone().with_workspace_changelog(path.into()),
                    &PackagesUpdate::new(vec![])
                )
                .is_err()
            );
        }
    }

    #[test]
    fn rollup_uses_release_date_after_version_metadata() {
        let root = std::env::current_dir().unwrap();
        let source = Utf8PathBuf::from_path_buf(root.join("a/CHANGELOG.md")).unwrap();
        let output = Utf8PathBuf::from_path_buf(root.join("docs/WORKSPACE.md")).unwrap();
        let text = "# Changelog\n\n## [1.0.0+2025-01-01] - 2026-09-05\n\n- Change\n";
        let rendered = render(&[("a".into(), source, text.into())], &output).unwrap();
        assert!(rendered.contains("## 2026-09-05\n"));
        assert!(!rendered.contains("## 2025-01-01\n"));
    }

    #[test]
    fn rollup_rejects_shared_source_changelogs() {
        let (_temp, request) = fixture();
        let request = request.with_default_package_config(UpdateConfig {
            changelog_path: Some("SHARED.md".into()),
            ..Default::default()
        });
        let error = prepare(&request, &PackagesUpdate::new(vec![])).unwrap_err();
        assert!(error.to_string().contains("independent package changelogs"));
    }

    #[test]
    fn rollup_uses_temporary_checkout_paths_after_manifest_relocation() {
        let (_original, request) = fixture();
        let (_copy, copied) = fixture();
        let original_root = request.local_manifest().parent().unwrap();
        let copied_root = copied.local_manifest().parent().unwrap();
        fs_err::write(
            original_root.join("a/CHANGELOG.md"),
            "# Changelog\n\n## [1.0.0]\n\n- Original checkout.\n",
        )
        .unwrap();
        fs_err::write(
            copied_root.join("a/CHANGELOG.md"),
            "# Changelog\n\n## [1.0.0]\n\n- Temporary checkout.\n",
        )
        .unwrap();
        let relocated = request.set_local_manifest(copied.local_manifest()).unwrap();
        let (path, text) = prepare(&relocated, &PackagesUpdate::new(vec![]))
            .unwrap()
            .unwrap();
        assert_eq!(path, copied_root.join("docs/WORKSPACE.md"));
        assert!(text.contains("Temporary checkout."));
        assert!(!text.contains("Original checkout."));
        assert!(text.contains("../a/CHANGELOG.md"));
        let original_package = cargo_utils::workspace_members(relocated.cargo_metadata())
            .unwrap()
            .find(|p| p.name.as_str() == "a")
            .unwrap();
        let pending = PackagesUpdate::new(vec![(
            original_package,
            UpdateResult {
                version: "1.1.0".parse().unwrap(),
                changelog: Some("# Changelog\n\n## [1.1.0]\n\n- Pending update.\n".into()),
                new_changelog_entry: Some("Pending update.".into()),
                registry_version: None,
                semver_check: SemverCheck::Skipped,
            },
        )]);
        let (_, planned) = prepare(&relocated, &pending).unwrap().unwrap();
        assert!(planned.contains("Pending update."));
        assert!(planned.contains("### a 1.1.0"));
        assert!(
            prepare(
                &relocated.with_workspace_changelog("a/CHANGELOG.md".into()),
                &PackagesUpdate::new(vec![])
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollup_rejects_symlink_alias_of_package_changelog() {
        let (_temp, request) = fixture();
        let root = request.local_manifest().parent().unwrap();
        fs_err::write(root.join("a/CHANGELOG.md"), "# Changelog\n").unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("alias")).unwrap();
        assert!(
            prepare(
                &request.with_workspace_changelog("alias/CHANGELOG.md".into()),
                &PackagesUpdate::new(vec![])
            )
            .is_err()
        );
    }

    #[test]
    fn rollup_resolves_reference_links_from_complete_history() {
        let notes = "[Guide][guide] and [Issue][issue].\n";
        let history = format!(
            "# Changelog\n\n## [1.0.0]\n\n{notes}\n[guide]: guide.md#api\n[issue]: https://example.com/1\n"
        );
        let root = std::env::current_dir().unwrap();
        let source = Utf8PathBuf::from_path_buf(root.join("a/CHANGELOG.md")).unwrap();
        let output = Utf8PathBuf::from_path_buf(root.join("docs/WORKSPACE.md")).unwrap();
        let rendered = relocate_notes(notes, &history, &source, &output).unwrap();
        assert!(rendered.contains("../a/guide.md#api"), "{rendered}");
        assert!(rendered.contains("https://example.com/1"), "{rendered}");
    }

    #[test]
    fn rollup_keeps_manual_text_and_rejects_broken_markers() {
        let manual = "# Overview\n\nMaintainer introduction.\n";
        let first = replace_generated(manual, "## 2026-09-05\n\nA\n").unwrap();
        let first = format!("{first}\nManual footer.\n");
        let second = replace_generated(&first, "## 2026-09-05\n\nB\n").unwrap();
        assert!(second.starts_with(manual));
        assert!(second.ends_with("Manual footer.\n"));
        assert!(!second.contains("\nA\n"));
        assert_eq!(second.matches(START).count(), 1);
        assert!(replace_generated(START, "test").is_err());
    }

    #[test]
    fn rollup_preserves_marker_examples_in_manual_code() {
        let manual = format!(
            "# Overview\n\nMarker example: `{START}`.\n\n```html\n{START}\nKeep this example.\n{END}\n```\n"
        );
        let first = replace_generated(&manual, "Generated content.\n").unwrap();
        assert!(first.starts_with(&manual));
        let second = replace_generated(&first, "Updated content.\n").unwrap();
        assert!(second.starts_with(&manual));
        assert!(!second.contains("Generated content."));
        assert!(second.contains("Updated content."));
    }

    #[test]
    fn rollup_relocates_links_without_changing_code_or_external_urls() {
        let notes = "### Fixed\n\n[Guide](guide.md#api) ![Image](img.png) [Issue](https://example.com/1)\n\n```md\n### This stays literal\n[Guide](guide.md)\n```\n";
        let root = std::env::current_dir().unwrap();
        let source = Utf8PathBuf::from_path_buf(root.join("a/CHANGELOG.md")).unwrap();
        let output = Utf8PathBuf::from_path_buf(root.join("docs/WORKSPACE.md")).unwrap();
        let rendered = relocate_notes(notes, notes, &source, &output).unwrap();
        assert!(rendered.contains("#### Fixed"));
        assert!(rendered.contains("../a/guide.md#api"));
        assert!(rendered.contains("../a/img.png"));
        assert!(rendered.contains("https://example.com/1"));
        assert!(rendered.contains("### This stays literal\n[Guide](guide.md)"));
    }
}
