use std::{
    path::Path,
    process::{Command, Output},
};

use tempfile::TempDir;

use super::super::{
    Plan, prepare,
    tests::{plan, repository, stage_all},
};

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installers_only_cover_available_platforms_and_are_reproducible() {
    for (targets, names) in [
        (vec!["x86_64-unknown-linux-musl"], vec!["demo-installer.sh"]),
        (vec!["aarch64-pc-windows-msvc"], vec!["demo-installer.ps1"]),
        (vec!["wasm32-wasip1"], vec![]),
    ] {
        let mut plan = plan();
        plan.targets = targets.into_iter().map(str::to_owned).collect();
        let directory = stage_all(&plan);
        let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
        assert_eq!(
            prepared
                .manifest
                .installers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            names
        );
        let first = fs_err::read(directory.path().join("dist-manifest.json")).unwrap();
        prepare(&plan, directory.path(), &repository()).unwrap();
        assert_eq!(
            fs_err::read(directory.path().join("dist-manifest.json")).unwrap(),
            first
        );
    }
}

struct Installation {
    archives: TempDir,
    root: TempDir,
}

impl Installation {
    fn new(plan: &Plan) -> Self {
        let archives = stage_all(plan);
        prepare(plan, archives.path(), &repository()).unwrap();
        let root = tempfile::Builder::new()
            .prefix("installer 'space ")
            .tempdir()
            .unwrap();
        for name in ["home", "tmp", "bin", "tools"] {
            fs_err::create_dir(root.path().join(name)).unwrap();
        }
        Self { archives, root }
    }

    fn configure(&self, command: &mut Command) {
        command
            .current_dir(self.root.path())
            .env("DIST_ARCHIVES", self.archives.path())
            .env("DIST_DOWNLOAD_LOG", self.root.path().join("download.log"))
            .env("RELEASE_PLZ_INSTALL_DIR", self.root.path().join("bin"))
            .env("HOME", self.root.path().join("home"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"));
    }

    fn assert_installed(&self, directory: &Path, windows: bool) {
        for binary in ["demo", "helper"] {
            let name = if windows {
                format!("{binary}.exe")
            } else {
                binary.to_owned()
            };
            assert_eq!(
                fs_err::read_to_string(directory.join(name)).unwrap(),
                format!("executable {binary}")
            );
        }
        assert_eq!(fs_err::read_dir(directory).unwrap().count(), 2);
        assert_eq!(
            fs_err::read_dir(self.root.path().join("tmp"))
                .unwrap()
                .count(),
            0
        );
    }

    fn downloaded_url(&self) -> String {
        fs_err::read_to_string(self.root.path().join("download.log")).unwrap()
    }
}

#[cfg(unix)]
mod shell {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    impl Installation {
        fn shell(&self, os: &str, arch: &str, libc: &str) -> Command {
            let tools = self.root.path().join("tools");
            for (name, source) in [
                (
                    "uname",
                    "case \"$1\" in -s) printf '%s' \"$DIST_OS\" ;; -m) printf '%s' \"$DIST_ARCH\" ;; esac",
                ),
                ("ldd", "printf '%s' \"$DIST_LIBC\""),
                ("sysctl", "printf '%s' \"${DIST_ROSETTA:-0}\""),
                (
                    "curl",
                    "while [ $# -gt 0 ]; do case \"$1\" in --output) shift; output=$1 ;; *) url=$1 ;; esac; shift; done\nprintf '%s\\n' \"$url\" > \"$DIST_DOWNLOAD_LOG\"\ncp \"$DIST_ARCHIVES/${url##*/}\" \"$output\"",
                ),
            ] {
                let path = tools.join(name);
                fs_err::write(&path, format!("#!/bin/sh\nset -eu\n{source}\n")).unwrap();
                fs_err::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let paths = std::iter::once(tools)
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
                .collect::<Vec<_>>();
            let mut command = Command::new("sh");
            self.configure(&mut command);
            command.env("PATH", std::env::join_paths(paths).unwrap())
                .env("DIST_OS", os).env("DIST_ARCH", arch).env("DIST_LIBC", libc)
                // Read stdin like `curl ... | sh`, rather than requiring a script file argument.
                .stdin(std::fs::File::open(self.archives.path().join("demo-installer.sh")).unwrap());
            command
        }
    }

    #[test]
    fn installs_matching_archives_for_each_unix_platform() {
        for (os, arch, libc, rosetta, target) in [
            (
                "Linux",
                "x86_64",
                "GNU libc",
                "0",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "Linux",
                "aarch64",
                "musl libc",
                "0",
                "aarch64-unknown-linux-musl",
            ),
            ("Darwin", "arm64", "", "0", "aarch64-apple-darwin"),
            ("Darwin", "x86_64", "", "0", "x86_64-apple-darwin"),
            ("Darwin", "x86_64", "", "1", "aarch64-apple-darwin"),
            ("FreeBSD", "amd64", "", "0", "x86_64-unknown-freebsd"),
        ] {
            let mut plan = plan();
            plan.targets = [
                "x86_64-unknown-linux-gnu",
                "x86_64-unknown-linux-musl",
                "aarch64-unknown-linux-gnu",
                "aarch64-unknown-linux-musl",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
                "x86_64-unknown-freebsd",
            ]
            .map(str::to_owned)
            .to_vec();
            // Quote characters and shell syntax in release data must remain literal.
            plan.tag = "v'$(touch${IFS}pwned)'@@CONFIG@@".to_owned();
            let installation = Installation::new(&plan);
            let output = installation
                .shell(os, arch, libc)
                .env("DIST_ROSETTA", rosetta)
                .output()
                .unwrap();
            assert_success(&output);
            let bin = installation.root.path().join("bin");
            installation.assert_installed(&bin, false);
            assert_eq!(
                fs_err::metadata(bin.join("demo"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert!(
                installation
                    .downloaded_url()
                    .trim()
                    .ends_with(&format!("demo-{target}.tar.gz"))
            );
            assert!(!installation.root.path().join("pwned").exists());
        }
    }

    #[test]
    fn defaults_to_local_bin_and_falls_back_to_static_musl() {
        let mut plan = plan();
        plan.targets = vec!["x86_64-unknown-linux-musl".to_owned()];
        let installation = Installation::new(&plan);
        let output = installation
            .shell("Linux", "x86_64", "GNU libc")
            .env_remove("RELEASE_PLZ_INSTALL_DIR")
            .output()
            .unwrap();
        assert_success(&output);
        installation.assert_installed(&installation.root.path().join("home/.local/bin"), false);
    }

    #[test]
    fn refuses_incompatible_platforms_without_downloading() {
        let installation = Installation::new(&plan());
        for (os, arch, libc) in [
            ("Linux", "x86_64", "musl libc"),
            ("Linux", "aarch64", "GNU libc"),
            ("Linux", "i686", "GNU libc"),
            ("Haiku", "x86_64", ""),
        ] {
            let output = installation.shell(os, arch, libc).output().unwrap();
            assert!(!output.status.success());
            assert!(!installation.root.path().join("download.log").exists());
            assert_eq!(
                fs_err::read_dir(installation.root.path().join("bin"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn checksum_failure_preserves_installed_binaries_and_cleans_up() {
        let installation = Installation::new(&plan());
        let binary = installation.root.path().join("bin/demo");
        fs_err::write(&binary, "previous version").unwrap();
        fs_err::write(
            installation
                .archives
                .path()
                .join("demo-x86_64-unknown-linux-gnu.tar.gz"),
            "corrupt",
        )
        .unwrap();
        let output = installation
            .shell("Linux", "x86_64", "GNU libc")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 checksum mismatch"));
        assert_eq!(fs_err::read_to_string(binary).unwrap(), "previous version");
        assert_eq!(
            fs_err::read_dir(installation.root.path().join("bin"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            fs_err::read_dir(installation.root.path().join("tmp"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn upgrade_replaces_symlink_without_overwriting_its_target() {
        let installation = Installation::new(&plan());
        let elsewhere = installation.root.path().join("other-executable");
        fs_err::write(&elsewhere, "keep me").unwrap();
        std::os::unix::fs::symlink(&elsewhere, installation.root.path().join("bin/demo")).unwrap();
        let output = installation
            .shell("Linux", "x86_64", "GNU libc")
            .output()
            .unwrap();
        assert_success(&output);
        installation.assert_installed(&installation.root.path().join("bin"), false);
        assert_eq!(fs_err::read_to_string(elsewhere).unwrap(), "keep me");
    }
}

#[test]
fn powershell_installs_verifies_checksums_and_cleans_up() {
    let program = if cfg!(windows) { "powershell" } else { "pwsh" };
    match Command::new(program).arg("-Version").output() {
        Err(error) if !cfg!(windows) && error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("PowerShell is not installed; run this test on Windows or with pwsh in PATH");
            return;
        }
        result => assert_success(&result.unwrap()),
    }
    let mut plan = plan();
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    plan.targets = vec![format!("{arch}-pc-windows-msvc")];
    plan.tag = "v'$(New-Item${IFS}pwned)'@@CONFIG@@".to_owned();
    let installation = Installation::new(&plan);
    let wrapper = installation.root.path().join("test.ps1");
    fs_err::write(
        &wrapper,
        r#"
$ErrorActionPreference = 'Stop'
function Invoke-WebRequest {
    param([switch] $UseBasicParsing, [string] $Uri, [string] $OutFile)
    Set-Content -LiteralPath $env:DIST_DOWNLOAD_LOG -Value $Uri
    $name = [System.Uri]::UnescapeDataString(([System.Uri]$Uri).Segments[-1])
    Copy-Item -LiteralPath (Join-Path $env:DIST_ARCHIVES $name) -Destination $OutFile
}
& (Join-Path $env:DIST_ARCHIVES 'demo-installer.ps1')
"#,
    )
    .unwrap();
    let run = || {
        let mut command = Command::new(program);
        installation.configure(&mut command);
        command
            .env("OS", "Windows_NT")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&wrapper)
            .output()
            .unwrap()
    };
    assert_success(&run());
    let bin = installation.root.path().join("bin");
    installation.assert_installed(&bin, true);
    assert!(
        installation
            .downloaded_url()
            .trim()
            .ends_with(&format!("demo-{arch}-pc-windows-msvc.zip"))
    );
    assert!(!installation.root.path().join("pwned").exists());
    // A second invocation can upgrade existing executables.
    fs_err::write(bin.join("demo.exe"), "old version").unwrap();
    assert_success(&run());
    installation.assert_installed(&bin, true);
    fs_err::write(bin.join("demo.exe"), "previous version").unwrap();
    fs_err::write(
        installation
            .archives
            .path()
            .join(format!("demo-{arch}-pc-windows-msvc.zip")),
        "corrupt",
    )
    .unwrap();
    let output = run();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 checksum mismatch"));
    assert_eq!(
        fs_err::read_to_string(bin.join("demo.exe")).unwrap(),
        "previous version"
    );
    assert_eq!(
        fs_err::read_dir(installation.root.path().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}
