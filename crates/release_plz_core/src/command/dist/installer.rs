//! Small, self-contained installers pinned to the archives validated at publication.

use std::{collections::BTreeMap, fmt::Write as _, path::Path};

use url::Url;

use super::{Artifact, Installer, Plan, digest, publish::download_url};

#[cfg(test)]
mod tests;

pub(super) fn generate(
    plan: &Plan,
    artifacts: &BTreeMap<String, Artifact>,
    directory: &Path,
    base: &Url,
) -> anyhow::Result<BTreeMap<String, Installer>> {
    let mut installers = BTreeMap::new();
    for windows in [false, true] {
        let mut entries = String::new();
        for (name, artifact) in artifacts {
            if !supported_target(&artifact.target, windows)
                || !name.ends_with(if windows { ".zip" } else { ".tar.gz" })
            {
                continue;
            }
            let arch = artifact.target.split_once('-').unwrap().0;
            if windows
                && ["msvc", "gnu", "gnullvm"]
                    .iter()
                    .find_map(|abi| {
                        let target = format!("{arch}-pc-windows-{abi}");
                        plan.targets.contains(&target).then_some(target)
                    })
                    .as_deref()
                    != Some(&artifact.target)
            {
                continue;
            }
            let url = download_url(base, &plan.tag, name)?.to_string();
            if windows {
                writeln!(
                    entries,
                    "        '{arch}' {{ $url = {}; $checksum = '{}'; break }}",
                    powershell_quote(&url),
                    artifact.sha256
                )?;
            } else {
                writeln!(
                    entries,
                    "        {}) url={}; checksum='{}' ;;",
                    artifact.target,
                    shell_quote(&url),
                    artifact.sha256
                )?;
            }
        }
        if entries.is_empty() {
            continue;
        }
        // Exactly one template substitution: release data cannot become template syntax.
        let (extension, script) = if windows {
            let binaries = plan
                .binaries
                .iter()
                .map(|b| powershell_quote(&format!("{b}.exe")))
                .collect::<Vec<_>>()
                .join(", ");
            let config = format!(
                "    $binaries = @({binaries})\n    switch ($arch) {{\n{entries}        default {{ throw \"No Windows archive for $arch in this release.\" }}\n    }}"
            );
            (
                "ps1",
                include_str!("installer.ps1").replace("@@CONFIG@@", &config),
            )
        } else {
            let binaries = plan
                .binaries
                .iter()
                .map(|b| shell_quote(b))
                .collect::<Vec<_>>()
                .join(" ");
            let config = format!(
                "    set -- {binaries}\n    select_archive() {{\n        target=$1\n        case \"$target\" in\n{entries}        *) return 1 ;;\n        esac\n    }}"
            );
            (
                "sh",
                include_str!("installer.sh").replace("@@CONFIG@@", &config),
            )
        };
        let name = format!("{}-installer.{extension}", plan.package);
        let path = directory.join(&name);
        fs_err::write(&path, script)?;
        let (sha256, size) = digest(&path)?;
        installers.insert(name, Installer { sha256, size });
    }
    Ok(installers)
}

fn supported_target(target: &str, windows: bool) -> bool {
    let Some((arch, platform)) = target.split_once('-') else {
        return false;
    };
    matches!(arch, "x86_64" | "aarch64")
        && if windows {
            matches!(
                platform,
                "pc-windows-msvc" | "pc-windows-gnu" | "pc-windows-gnullvm"
            )
        } else {
            matches!(
                platform,
                "unknown-linux-gnu" | "unknown-linux-musl" | "apple-darwin" | "unknown-freebsd"
            )
        }
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(super) fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
