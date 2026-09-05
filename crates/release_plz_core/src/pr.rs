use crate::{
    PackagesUpdate, ReleaseInfo,
    tera::{PACKAGE_VAR, RELEASES_VAR, VERSION_VAR, render_template},
};
use chrono::SecondsFormat;

pub const DEFAULT_BRANCH_PREFIX: &str = "release-plz-";
pub const OLD_BRANCH_PREFIX: &str = "release-plz/";
pub const DEFAULT_PR_BODY_TEMPLATE: &str = r#"
{% set changes %}
{%- for release in releases %}
{%- if release.changelog %}{% if releases | length > 1 %}
## `{{ release.package }}`
{% endif %}
<blockquote>

{% if release.title %}## {{ release.title }}
{% endif %}
{{ release.changelog }}
</blockquote>{% endif %}
{% endfor %}
{% endset %}

## 🤖 New release
{% for release in releases %}
* `{{ release.package }}`: {% if release.previous_version and release.previous_version != release.next_version %}{{ release.previous_version }} -> {% endif %}{{ release.next_version }}{% if release.semver_check == "incompatible" %} (⚠ API breaking changes){% elif release.semver_check == "compatible" %} (✓ API compatible changes){% endif %}
{%- endfor %}
{%- for release in releases %}{% if release.breaking_changes %}

### ⚠ `{{ release.package }}` breaking changes

```text
{{ release.breaking_changes }}
```{% endif %}{% endfor %}
{% if changes %}
<details><summary><i><b>Changelog</b></i></summary><p>
{{ changes }}
</p></details>
{% endif %}
---
This PR was generated with [release-plz](https://github.com/release-plz/release-plz/)."#;

#[derive(Debug)]
pub struct Pr {
    pub base_branch: String,
    pub branch: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub labels: Vec<String>,
}

impl Pr {
    pub fn new(
        default_branch: &str,
        packages_to_update: &PackagesUpdate,
        project_contains_multiple_pub_packages: bool,
        branch_prefix: &str,
        title_template: Option<String>,
        body_template: Option<&str>,
    ) -> anyhow::Result<Self> {
        let pr = Self {
            branch: release_branch(branch_prefix),
            base_branch: default_branch.to_string(),
            title: pr_title(
                packages_to_update,
                project_contains_multiple_pub_packages,
                title_template,
            )?,
            body: pr_body(packages_to_update, body_template)?,
            draft: false,
            labels: vec![],
        };
        Ok(pr)
    }

    pub fn mark_as_draft(mut self, draft: bool) -> Self {
        self.draft = draft;
        self
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }
}

/// Resolve identity from the base checkout, never from the newly calculated bump.
pub(crate) fn render_branch_prefix(
    template: &str,
    branch: &str,
    packages: &[cargo_metadata::Package],
) -> anyhow::Result<String> {
    let mut context = tera::Context::new();
    context.insert("branch", branch);
    if let [package] = packages {
        context.insert(PACKAGE_VAR, package.name.as_str());
        context.insert(VERSION_VAR, &package.version.to_string());
    }
    let prefix = render_template(template, &context, "pr_branch_prefix")?;
    anyhow::ensure!(
        !prefix.is_empty()
            && git2::Reference::is_valid_name(&format!("refs/heads/{prefix}release")),
        "pr_branch_prefix must render to a nonempty valid Git branch prefix"
    );
    Ok(prefix)
}

pub(crate) fn prefix_marker(template: &str, branch: &str) -> String {
    use base64::Engine as _;
    let identity = serde_json::to_vec(&(template, branch)).expect("strings serialize");
    format!(
        "<!-- release-plz-prefix:{} -->",
        base64::engine::general_purpose::STANDARD.encode(identity)
    )
}

pub(crate) fn matches_release_prefix(
    template: &str,
    branch: &str,
    head: &str,
    body: Option<&str>,
) -> bool {
    if is_prefix_template(template) {
        body.is_some_and(|body| body.contains(&prefix_marker(template, branch)))
    } else {
        head.starts_with(template)
    }
}

pub(crate) fn is_prefix_template(prefix: &str) -> bool {
    prefix.contains("{{") || prefix.contains("{%")
}

fn release_branch(prefix: &str) -> String {
    let now = chrono::offset::Utc::now();
    // Convert to a string of format "2018-01-26T18:30:09Z".
    let now = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    // ':' is not a valid character for a branch name.
    let now = now.replace(':', "-");
    format!("{prefix}{now}")
}

fn pr_title(
    packages_to_update: &PackagesUpdate,
    project_contains_multiple_pub_packages: bool,
    title_template: Option<String>,
) -> anyhow::Result<String> {
    let updates = packages_to_update.updates();
    let first_version = &updates[0].1.version;

    let are_all_versions_equal = || {
        updates
            .iter()
            .all(|(_, update)| &update.version == first_version)
    };

    let title = if let Some(title_template) = title_template {
        let mut context = tera::Context::new();

        if updates.len() == 1 {
            let (package, _) = &updates[0];
            context.insert(PACKAGE_VAR, &package.name);
        }

        if are_all_versions_equal() {
            context.insert(VERSION_VAR, first_version.to_string().as_str());
        }

        render_template(&title_template, &context, "pr_name")?
    } else if updates.len() == 1 && project_contains_multiple_pub_packages {
        let (package, _) = &updates[0];
        // The project is a workspace with multiple public packages and we are only updating one of them.
        // Specify which package is being updated in the PR title.
        format!("chore({}): release v{}", package.name, first_version)
    } else if updates.len() > 1 && !are_all_versions_equal() {
        // We are updating multiple packages with different versions, so we don't specify the version in the PR title.
        "chore: release".to_string()
    } else {
        // We are updating either:
        // - a single package without other public packages
        // - multiple packages with the same version.
        // In both cases, we can specify the version in the PR title.
        format!("chore: release v{first_version}")
    };
    Ok(title)
}

/// The Github API allows a max of 65536 characters in the body field when trying to create a new PR
const MAX_BODY_LEN: usize = 65536;

fn pr_body(
    packages_to_update: &PackagesUpdate,
    body_template: Option<&str>,
) -> anyhow::Result<String> {
    let body_template = body_template.unwrap_or(DEFAULT_PR_BODY_TEMPLATE);

    let mut releases = packages_to_update.releases();
    let first_render = render_pr_body(&releases, body_template)?;

    if first_render.chars().count() > MAX_BODY_LEN {
        tracing::info!(
            "PR body is longer than {MAX_BODY_LEN} characters. Omitting full changelog."
        );

        releases.iter_mut().for_each(|release| {
            release.changelog = None;
            release.title = None;
        });

        render_pr_body(&releases, body_template)
    } else {
        Ok(first_render)
    }
}

fn render_pr_body(releases: &[ReleaseInfo], body_template: &str) -> anyhow::Result<String> {
    let mut context = tera::Context::new();
    context.insert(RELEASES_VAR, releases);

    let rendered_body = render_template(body_template, &context, "pr_body")?;
    Ok(trim_pr_body(rendered_body))
}

fn trim_pr_body(body: String) -> String {
    // Make extra sure the body is short enough.
    // If it's not, give up trying to fail gracefully by truncating it to the nearest valid UTF-8 boundary.
    // A grapheme cluster may be cut in half in the process.

    if body.chars().count() > MAX_BODY_LEN {
        tracing::warn!("PR body is still longer than {MAX_BODY_LEN} characters. Truncating as is.");
        body.chars().take(MAX_BODY_LEN).collect()
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_templates_use_stable_base_metadata_and_reject_ambiguous_packages() {
        let package: cargo_metadata::Package = fake_package::FakePackage::new("MyCrate").into();
        let template = "release-{{ branch }}-{{ package }}-{{ version }}-";
        let prefix =
            render_branch_prefix(template, "0.8.x", std::slice::from_ref(&package)).unwrap();
        assert_eq!(prefix, "release-0.8.x-MyCrate-0.1.0-");
        assert!(release_branch(&prefix).starts_with(&prefix));
        assert!(render_branch_prefix(template, "main", &[package.clone(), package]).is_err());
        assert!(render_branch_prefix("", "main", &[]).is_err());
        assert!(render_branch_prefix("bad prefix-", "main", &[]).is_err());
        assert_ne!(
            prefix_marker(template, "main"),
            prefix_marker(template, "0.8.x")
        );
    }

    #[test]
    fn merged_prefix_identity_survives_version_change_but_not_branch_change() {
        let template = "release-{{ version }}-";
        let body = prefix_marker(template, "main");
        assert!(matches_release_prefix(
            template,
            "main",
            "release-0.1.0-2026-01-01",
            Some(&body)
        ));
        assert!(!matches_release_prefix(
            template,
            "0.8.x",
            "release-0.1.0-2026-01-01",
            Some(&body)
        ));
        assert!(!matches_release_prefix(
            "different-{{ version }}-",
            "main",
            "release-0.1.0-2026-01-01",
            Some(&body)
        ));
    }

    #[test]
    fn default_pr_body_template_renders() {
        let releases = serde_json::json!([{
            "package": "my-package",
            "title": "[0.1.1] - 2026-06-27",
            "changelog": "### Other\n\n- fixed a bug",
            "previous_version": "0.1.0",
            "next_version": "0.1.1",
            "breaking_changes": null,
            "semver_check": "compatible",
        }]);
        let mut context = tera::Context::new();
        context.insert(RELEASES_VAR, &releases);

        let body = render_template(DEFAULT_PR_BODY_TEMPLATE, &context, "pr_body").unwrap();

        assert!(body.contains("## 🤖 New release"));
        assert!(body.contains("* `my-package`: 0.1.0 -> 0.1.1 (✓ API compatible changes)"));
        assert!(body.contains("- fixed a bug"));
    }
}
