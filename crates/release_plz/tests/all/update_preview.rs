use std::{path::Path, process::Command};

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(directory)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn update_preview_preserves_files_and_reports_pending_releases() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs_err::create_dir(root.join("src")).unwrap();
    fs_err::write(root.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
    fs_err::write(root.join("Cargo.toml"), "[package]\nname=\"update-preview-fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\nlicense=\"MIT\"\ndescription=\"Preview test\"\n").unwrap();
    fs_err::write(
        root.join("release-plz.toml"),
        "[workspace]\ngit_only=true\nsemver_check=false\n",
    )
    .unwrap();
    git(root, &["init", "-q"]);
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/update-preview-fixture",
        ],
    );
    let status = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    git(root, &["tag", "v0.1.0"]);
    fs_err::write(
        root.join("src/lib.rs"),
        "pub fn original() {}\npub fn added() {}\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "fix: add function"]);

    let manifest = fs_err::read(root.join("Cargo.toml")).unwrap();
    let lock = fs_err::read(root.join("Cargo.lock")).unwrap();
    for (flag, expected_code) in [("--dry-run", 0), ("--check", 1)] {
        let result = assert_cmd::cargo::cargo_bin_cmd!("release-plz")
            .current_dir(root)
            .args(["update", flag])
            .assert()
            .code(expected_code);
        assert!(String::from_utf8_lossy(&result.get_output().stdout).contains("0.1.1"));
        assert_eq!(fs_err::read(root.join("Cargo.toml")).unwrap(), manifest);
        assert_eq!(fs_err::read(root.join("Cargo.lock")).unwrap(), lock);
        assert!(!root.join("CHANGELOG.md").exists());
    }
    // Mark the changed source as released, so check mode has no pending release.
    git(root, &["tag", "-f", "v0.1.0"]);
    assert_cmd::cargo::cargo_bin_cmd!("release-plz")
        .current_dir(root)
        .args(["update", "--check"])
        .assert()
        .success();
}

#[test]
fn release_pr_rejects_preview_options_before_accessing_a_forge() {
    let result = assert_cmd::cargo::cargo_bin_cmd!("release-plz")
        .args(["release-pr", "--check"])
        .assert()
        .failure();
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr)
            .contains("only supported by the update command")
    );
}
