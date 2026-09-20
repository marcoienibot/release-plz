use std::{path::Path, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

fn run(directory: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn builds_a_real_binary_and_prepares_an_offline_release() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // Exercise Cargo's actual executable paths, including a target-dir containing spaces.
    let build_cache = tempfile::Builder::new()
        .prefix("dist build cache ")
        .tempdir()
        .unwrap();
    fs_err::create_dir(root.join("src")).unwrap();
    fs_err::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "dist-example"
version = "1.2.3"
edition = "2024"

[features]
extra = []

[[bin]]
name = "helper"
path = "src/helper.rs"
required-features = ["extra"]
"#,
    )
    .unwrap();
    fs_err::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"distributed\"); }\n",
    )
    .unwrap();
    fs_err::write(root.join("src/helper.rs"), "fn main() {}\n").unwrap();
    fs_err::write(root.join(".gitignore"), "target/\n").unwrap();
    let rustc = run(root, "rustc", &["-vV"]);
    let target = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap();
    run(root, "cargo", &["generate-lockfile", "--offline"]);
    run(root, "git", &["init"]);
    run(root, "git", &["add", "."]);
    run(
        root,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ],
    );
    run(
        root,
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "tag",
            "-a",
            "v1.2.3",
            "-m",
            "release",
        ],
    );

    let command = || {
        let mut command = cargo_bin_cmd!("release-plz");
        command
            .current_dir(root)
            .env("CARGO_TARGET_DIR", build_cache.path());
        command.env_remove("GITHUB_TOKEN");
        command.arg("dist");
        command
    };
    // The publisher must know the intended set, even when there is no config file.
    command()
        .args(["publish", "--tag", "v1.2.3", "--dry-run"])
        .assert()
        .failure()
        .code(2);
    let output = command()
        .args(["plan", "--tag", "v1.2.3", "--target", target])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let plan: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(plan["package"], "dist-example");
    assert_eq!(
        plan["binaries"],
        serde_json::json!(["dist-example", "helper"])
    );
    assert!(!root.join("target/distrib").exists());

    command()
        .args(["build", "--tag", "missing", "--target", target])
        .assert()
        .failure();
    command()
        .args(["build", "--tag", "v1.2.3", "--target", "unknown-target"])
        .assert()
        .failure();
    // Required-feature binaries must not silently disappear from the release.
    command()
        .args(["build", "--tag", "v1.2.3", "--target", target])
        .assert()
        .failure();
    let manifest_path = root.join(format!(
        "target/distrib/dist-example-{target}.dist-manifest.json"
    ));
    assert!(!manifest_path.exists());
    command()
        .args([
            "build",
            "--tag",
            "v1.2.3",
            "--target",
            target,
            "--features",
            "extra",
        ])
        .assert()
        .success();
    assert!(manifest_path.exists());
    // A successful build of one target cannot satisfy a larger CI matrix.
    let targets = format!("{target},missing-target");
    command()
        .args([
            "publish",
            "--tag",
            "v1.2.3",
            "--target",
            &targets,
            "--repo-url",
            "https://github.com/example/demo",
            "--dry-run",
        ])
        .assert()
        .failure();
    assert!(!root.join("target/distrib/dist-manifest.json").exists());
    command()
        .args([
            "publish",
            "--target",
            target,
            "--tag",
            "v1.2.3",
            "--repo-url",
            "https://github.com/example/demo",
            "--dry-run",
        ])
        .assert()
        .success();
    let manifest: Value = serde_json::from_slice(
        &fs_err::read(root.join("target/distrib/dist-manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["plan"], plan);
    let checksum = fs_err::read_to_string(root.join("target/distrib/sha256.sum")).unwrap();
    assert!(checksum.contains("dist-manifest.json"));
    assert!(checksum.contains(&format!("dist-example-{target}.tar.gz")));
    let notes = fs_err::read_to_string(root.join("target/distrib/dist-notes.md")).unwrap();
    assert!(notes.contains("## Downloads"));
    assert!(notes.contains("## Install"));
    let installer = format!(
        "dist-example-installer.{}",
        if cfg!(windows) { "ps1" } else { "sh" }
    );
    assert!(root.join("target/distrib").join(&installer).exists());
    assert!(manifest["installers"].get(&installer).is_some());
    assert!(checksum.contains(&installer));
    assert!(run(root, "git", &["status", "--porcelain"]).is_empty());

    // A build failure invalidates its old manifest, even after a prior success.
    command()
        .args([
            "build",
            "--tag",
            "v1.2.3",
            "--target",
            target,
            "--features",
            "not-a-feature",
        ])
        .assert()
        .failure();
    assert!(!manifest_path.exists());
    command()
        .args([
            "publish",
            "--target",
            target,
            "--tag",
            "v1.2.3",
            "--repo-url",
            "https://github.com/example/demo",
            "--dry-run",
        ])
        .assert()
        .failure();

    fs_err::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    command()
        .args([
            "build",
            "--tag",
            "v1.2.3",
            "--target",
            target,
            "--features",
            "extra",
        ])
        .assert()
        .failure();
}
