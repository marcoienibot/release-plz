# release-plz dist

`release-plz dist` builds Rust binary archives and publishes them to an existing
draft GitHub release. You own the CI workflow: choose runners, install toolchains,
and transfer build artifacts between jobs. Release-plz handles building,
packaging, checksums, validation, installation scripts, release notes, and publication.

## Configure

Configure draft releases in `release-plz.toml`:

```toml
[[package]]
name = "my-app"
git_release_enable = true
git_release_draft = true
git_release_latest = false
```

Targets are declared in CI and passed with `--target`; no distribution config
section is needed. Each build needs only its own target. Publication takes the
complete expected target list from the CI matrix so it can detect missing builds.
Inferring that list from the files received would silently accept a missing target.

One package is distributed per invocation; all its binary targets go into each
archive. Use `--package` when multiple workspace packages have binaries.
`--config` and `--manifest-path` work as on the other commands; configuration paths
are relative to the current directory.

## Plan

```sh
release-plz dist plan --tag my-app-v1.2.3 \
  --target x86_64-unknown-linux-gnu --target x86_64-pc-windows-msvc
```

Prints JSON containing the package, version, binaries, targets, tag and current
commit. This command does not build anything or require the tag to exist yet.
It can run in a pull request to validate package selection and targets. Repeat
`--target` or pass a comma-separated list.

## Build

Check out the release tag, then run once on each target's runner:

```sh
release-plz dist build --tag my-app-v1.2.3 --target x86_64-unknown-linux-gnu
```

The command requires a clean checkout of the tag and runs
`cargo build --release --locked --bins --target …`. Install the Rust target and
any required linker or SDK in your CI first. Use native runners or configure a
Cargo cross toolchain, including the target linker. This also applies to FreeBSD.
Release-plz always uses `cargo build`; it does not install system packages,
generate workflows, or select runners.

The defaults are:

- Flat `<package>-<target>.tar.gz` archives for every target, including Windows.
- An additional flat `<package>-<target>.zip` on Windows, with `.exe` binaries.
- Executable permissions in archives and the static C runtime on MSVC targets.
- SHA-256 checksum files next to archives.
- A `<package>-<target>.dist-manifest.json` describing the build and archive digests.

Archives contain only the package's executables, at the archive root. Cargo's
`release` profile controls optimization. Pass `--features feature1,feature2` or
`--no-default-features` when needed. A binary requiring a disabled feature causes
an error rather than an incomplete archive.

Output goes into `target/distrib` by default; `--output-dir` overrides it. Keep
that directory ignored by Git. Upload **all files** from it as CI artifacts. Each
target has distinct filenames, so the publish job can merge their directories.
Each build manifest records only its own target. It is written only after
successful packaging; failed rebuilds invalidate the previous target manifest.

## Publish

After every build succeeds, check out the same tag and download all the CI
artifacts into `target/distrib` on one runner. Pass the same target list used by
the build matrix, for example as a comma-separated `TARGETS` environment variable:

```sh
release-plz dist publish --tag my-app-v1.2.3 --target "$TARGETS" --dry-run
release-plz dist publish --tag my-app-v1.2.3 --target "$TARGETS"
```

`--dry-run` validates every target and generates installers, their checksums,
`dist-manifest.json`, `sha256.sum`, and `dist-notes.md` locally. It prints a JSON
summary and needs no token or GitHub access. The notes file previews the install
commands and download table that will be added to the existing release notes.
`--artifacts-dir` overrides the input/output directory.

Publication needs `GITHUB_TOKEN` or `--git-token` with `contents: write` permission.
The repository defaults to `workspace.repo_url`, then the Git origin; override
it with `--repo-url https://github.com/owner/repo`. GitHub Enterprise URLs are also
supported. Other forges are not supported by this command yet.

Release-plz checks the complete target list, package version, tag, commit, archive
names and digests before uploading. It also checks that the remote tag points at
the built commit. It replaces matching assets on the draft, uploads the archives,
installers, their checksums, combined manifest and `sha256.sum`, then publishes the
release. It preserves the existing changelog, contributors and prerelease status.
Stable releases become latest; prereleases do not. The download table has markers
so it can be updated without duplication.

If any build, validation or upload fails, the release stays draft. Retry with the
same tag and artifacts. An already published release is never modified. Serialize
publication jobs for the same tag in your CI to prevent concurrent publishers.

## Installation scripts

Publication automatically generates `<package>-installer.sh` for Linux, macOS
and FreeBSD, and `<package>-installer.ps1` for Windows when matching archives
exist. Installers support x86-64 and ARM64 and select only targets built for that
release. Other targets remain available as archive downloads. No additional
configuration or CI steps are needed.

The release notes include commands to install that specific release:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  'https://github.com/owner/repo/releases/download/my-app-v1.2.3/my-app-installer.sh' | sh
```

On Windows, use PowerShell 5.1 or later:

```powershell
irm 'https://github.com/owner/repo/releases/download/my-app-v1.2.3/my-app-installer.ps1' | iex
```

For the latest stable release, the URL can instead use
`/releases/latest/download/my-app-installer.sh` (or `.ps1`). The script still pins
its archive downloads and SHA-256 digests to the release it belongs to.

Installers verify the archive's SHA-256 before extracting and installing every
packaged executable into `~/.local/bin`. Existing executables with the same names
are replaced. Set `RELEASE_PLZ_INSTALL_DIR` to change the installation directory:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  'https://github.com/owner/repo/releases/latest/download/my-app-installer.sh' |
  RELEASE_PLZ_INSTALL_DIR="$HOME/bin" sh
```

```powershell
$env:RELEASE_PLZ_INSTALL_DIR = "$HOME/bin"
irm 'https://github.com/owner/repo/releases/latest/download/my-app-installer.ps1' | iex
```

The scripts print the installation directory; they do not modify `PATH` or shell
profiles. Add that directory to `PATH` if necessary. Downloads must be accessible
over HTTPS without authentication. Unix requires `curl`, `tar`, `mktemp`, and
one of `sha256sum`, `shasum`, or FreeBSD's `sha256`. PowerShell uses built-in
download, ZIP extraction and hashing commands.

On Linux, musl systems require a musl archive. GNU systems prefer a GNU archive
and can fall back to musl; musl builds must retain Rust's default static C runtime.
macOS detects Apple Silicon even when the shell runs under Rosetta. Windows
prefers MSVC archives, falling back to GNU or GNU LLVM archives if those are the
available builds. Installers report an error when no compatible archive exists.

## Connect to the release workflow

Run `release-plz release` first to publish crates, push tags and create drafts.
Then pass the package's tag from its [JSON output](../github/output.md) to your
build workflow. Call it after the release command finishes: draft creation does
not trigger a published-release event, and a tag-push job can race draft creation.

The workflow needs three pieces:

1. A build matrix running `dist build --target` for each target.
2. Artifact upload/download steps to collect all target outputs.
3. One job running `dist publish --target` with the complete matrix target list
   after every build succeeds.

See release-plz's own
[`cd.yml`](https://github.com/release-plz/release-plz/blob/main/.github/workflows/cd.yml)
and
[`release-plz.yml`](https://github.com/release-plz/release-plz/blob/main/.github/workflows/release-plz.yml)
for a maintained example. That workflow builds release-plz from the release tag
so the first version containing `dist` can distribute itself.
Its matrix job declares targets once and passes them to both the build jobs and
publication, so there is no second list to keep in sync.
