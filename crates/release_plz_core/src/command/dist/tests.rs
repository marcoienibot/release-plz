use std::io::Read as _;

use flate2::read::GzDecoder;
use serde_json::json;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, query_param},
};

use super::*;
use crate::{GitHub, RepoUrl};

pub(super) fn plan() -> Plan {
    Plan {
        schema_version: SCHEMA_VERSION,
        package: "demo".to_owned(),
        version: "1.2.3".to_owned(),
        tag: "demo-v1.2.3".to_owned(),
        commit: "0123456789abcdef".to_owned(),
        binaries: vec!["demo".to_owned(), "helper".to_owned()],
        targets: vec![
            "x86_64-pc-windows-msvc".to_owned(),
            "x86_64-unknown-freebsd".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
        ],
    }
}

pub(super) fn stage_all(plan: &Plan) -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let binaries = plan
        .binaries
        .iter()
        .map(|name| {
            let path = sources.path().join(name);
            fs_err::write(&path, format!("executable {name}")).unwrap();
            (name.clone(), path)
        })
        .collect();
    for target in &plan.targets {
        build::stage(plan, target, directory.path(), &binaries).unwrap();
    }
    directory
}

pub(super) fn repository() -> RepoUrl {
    RepoUrl::new("https://github.com/example/demo").unwrap()
}

#[test]
fn archive_names_layout_permissions_and_checksums_are_compatible() {
    let plan = plan();
    let directory = stage_all(&plan);
    for target in &plan.targets {
        let build_manifest: TargetManifest = serde_json::from_slice(
            &fs_err::read(directory.path().join(plan.target_manifest_name(target))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            build_manifest.plan.targets.as_slice(),
            std::slice::from_ref(target)
        );
        let windows = is_windows(target);
        let archive_path = directory.path().join(format!("demo-{target}.tar.gz"));
        let mut archive =
            tar::Archive::new(GzDecoder::new(fs_err::File::open(&archive_path).unwrap()));
        let mut contents = BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            assert_eq!(entry.header().mode().unwrap(), 0o755);
            let name = entry.path().unwrap().to_str().unwrap().to_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            contents.insert(name, bytes);
        }
        for binary in &plan.binaries {
            let filename = if windows {
                format!("{binary}.exe")
            } else {
                binary.clone()
            };
            assert_eq!(
                contents[&filename],
                format!("executable {binary}").as_bytes()
            );
        }
        assert_eq!(contents.len(), 2);
        if windows {
            let mut archive = zip::ZipArchive::new(
                fs_err::File::open(directory.path().join(format!("demo-{target}.zip"))).unwrap(),
            )
            .unwrap();
            assert_eq!(archive.len(), contents.len());
            for (name, expected) in contents {
                let mut bytes = Vec::new();
                archive
                    .by_name(&name)
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                assert_eq!(bytes, expected);
            }
        }
    }
    let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
    assert_eq!(prepared.manifest.artifacts.len(), 4);
    assert_eq!(prepared.manifest.installers.len(), 2);
    for (name, installer) in &prepared.manifest.installers {
        assert_eq!(
            digest(&directory.path().join(name)).unwrap(),
            (installer.sha256.clone(), installer.size)
        );
        assert!(prepared.files.contains(name));
        assert!(prepared.files.contains(&format!("{name}.sha256")));
    }
    assert!(prepared.download_notes.contains("curl --proto '=https'"));
    assert!(prepared.download_notes.contains(" | sh\n"));
    assert!(prepared.download_notes.contains("irm 'https://"));
    assert!(prepared.download_notes.contains(" | iex\n"));
    for line in fs_err::read_to_string(directory.path().join(CHECKSUMS_NAME))
        .unwrap()
        .lines()
    {
        let (expected, name) = line.split_once("  ").unwrap();
        assert_eq!(digest(&directory.path().join(name)).unwrap().0, expected);
    }
    assert!(
        prepared
            .download_notes
            .contains("demo-x86_64-unknown-freebsd.tar.gz")
    );
    assert!(
        prepared
            .download_notes
            .contains("https://github.com/example/demo/releases/download/demo-v1.2.3/")
    );
    assert!(
        !prepared
            .files
            .iter()
            .any(|name| name.ends_with(&format!(".{MANIFEST_NAME}")))
    );
}

#[test]
fn rejects_missing_corrupt_or_mismatched_artifacts() {
    let plan = plan();
    for failure in [
        "missing-target",
        "missing-archive",
        "corrupt",
        "tag",
        "commit",
        "version",
        "targets",
        "path",
        "schema",
    ] {
        let directory = stage_all(&plan);
        let target = &plan.targets[0];
        let manifest_path = directory.path().join(plan.target_manifest_name(target));
        let archive_name = &plan.archives(target)[0];
        match failure {
            "missing-target" => fs_err::remove_file(&manifest_path).unwrap(),
            "missing-archive" => fs_err::remove_file(directory.path().join(archive_name)).unwrap(),
            "corrupt" => fs_err::write(directory.path().join(archive_name), "broken").unwrap(),
            _ => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs_err::read(&manifest_path).unwrap()).unwrap();
                match failure {
                    "targets" => value["plan"]["targets"] = json!([]),
                    "schema" => value["plan"]["schema_version"] = json!(999),
                    "path" => {
                        let artifact = value["artifacts"]
                            .as_object_mut()
                            .unwrap()
                            .remove(archive_name)
                            .unwrap();
                        value["artifacts"]["../outside.tar.gz"] = artifact;
                    }
                    key => value["plan"][key] = json!("wrong"),
                }
                write_json(&manifest_path, &value).unwrap();
            }
        }
        assert!(
            prepare(&plan, directory.path(), &repository()).is_err(),
            "{failure}"
        );
        assert!(!directory.path().join(MANIFEST_NAME).exists(), "{failure}");
    }
}

fn release(server: &MockServer, draft: bool, prerelease: bool) -> serde_json::Value {
    json!({
        "id": 42,
        "tag_name": plan().tag,
        "draft": draft,
        "prerelease": prerelease,
        "body": "Changelog with `code`, $variables and 'quotes'.\n\n### Contributors\n* @alice",
        "upload_url": format!("{}/uploads{{?name,label}}", server.uri()),
    })
}

struct Upload;

impl wiremock::Respond for Upload {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let name = request
            .url
            .query_pairs()
            .find(|(key, _)| key == "name")
            .unwrap()
            .1
            .into_owned();
        ResponseTemplate::new(201).set_body_json(json!({
            "id": 99,
            "name": name,
            "size": request.body.len(),
            "state": "uploaded",
            "digest": format!("sha256:{:x}", Sha256::digest(&request.body)),
        }))
    }
}

async fn mock_release(server: &MockServer, draft: bool, prerelease: bool) {
    let value = release(server, draft, prerelease);
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([value])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/releases/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/commits/demo-v1.2.3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": plan().commit})))
        .mount(server)
        .await;
    Mock::given(method("GET")).and(path("/repos/example/demo/releases/42/assets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 7, "name": "demo-x86_64-pc-windows-msvc.tar.gz", "size": 0, "state": "starter", "digest": null
        }])))
        .mount(server).await;
    Mock::given(method("DELETE"))
        .and(path("/repos/example/demo/releases/assets/7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

fn github(server: &MockServer) -> GitHub {
    GitHub::new("example".to_owned(), "demo".to_owned(), "token".into())
        .with_base_url(server.uri().parse().unwrap())
}

#[tokio::test]
async fn publishes_only_after_uploads_and_preserves_notes_and_prerelease_status() {
    for prerelease in [false, true] {
        let server = MockServer::start().await;
        mock_release(&server, true, prerelease).await;
        Mock::given(method("POST"))
            .and(path("/uploads"))
            .respond_with(Upload)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/example/demo/releases/42"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let plan = plan();
        let directory = stage_all(&plan);
        let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
        publish(&github(&server), &prepared).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let uploads: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
        assert_eq!(uploads.len(), prepared.files.len());
        let last = requests.last().unwrap();
        assert_eq!(last.method, "PATCH");
        let body: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
        assert_eq!(body["draft"], false);
        assert_eq!(
            body["make_latest"],
            if prerelease { "false" } else { "true" }
        );
        assert!(body.get("prerelease").is_none());
        let notes = body["body"].as_str().unwrap();
        assert!(notes.starts_with(release(&server, true, prerelease)["body"].as_str().unwrap()));
        assert!(notes.contains("## Downloads"));
        let delete = requests.iter().position(|r| r.method == "DELETE").unwrap();
        let upload = requests
            .iter()
            .position(|r| {
                r.method == "POST"
                    && r.url.query_pairs().any(|(key, value)| {
                        key == "name" && value == "demo-x86_64-pc-windows-msvc.tar.gz"
                    })
            })
            .unwrap();
        assert!(delete < upload);
    }
}

#[tokio::test]
async fn failed_upload_leaves_draft_and_retry_succeeds() {
    let server = MockServer::start().await;
    mock_release(&server, true, false).await;
    Mock::given(method("POST"))
        .and(path("/uploads"))
        .respond_with(Upload)
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/uploads"))
        .and(query_param("name", CHECKSUMS_NAME))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let plan = plan();
    let directory = stage_all(&plan);
    let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
    assert!(publish(&github(&server), &prepared).await.is_err());
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method == "PATCH")
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        prepared.files.len()
    );
    Mock::given(method("PATCH"))
        .and(path("/repos/example/demo/releases/42"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    publish(&github(&server), &prepared).await.unwrap();
}

#[tokio::test]
async fn published_releases_are_never_modified() {
    let server = MockServer::start().await;
    mock_release(&server, false, false).await;
    let plan = plan();
    let directory = stage_all(&plan);
    let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
    let error = publish(&github(&server), &prepared).await.unwrap_err();
    assert!(error.to_string().contains("already published"));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}

#[tokio::test]
async fn archives_and_installers_changed_after_preparation_are_not_uploaded() {
    for name in ["demo-installer.ps1", "demo-x86_64-pc-windows-msvc.tar.gz"] {
        let server = MockServer::start().await;
        mock_release(&server, true, false).await;
        Mock::given(method("POST"))
            .and(path("/uploads"))
            .respond_with(Upload)
            .mount(&server)
            .await;
        let plan = plan();
        let directory = stage_all(&plan);
        let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
        fs_err::write(directory.path().join(name), "changed").unwrap();
        let error = publish(&github(&server), &prepared).await.unwrap_err();
        assert!(error.to_string().contains("changed after validation"));
        let requests = server.received_requests().await.unwrap();
        assert!(
            !requests
                .iter()
                .any(|r| r.method == "PATCH" || r.method == "DELETE")
        );
        assert!(!requests.iter().any(|r| {
            r.method == "POST"
                && r.url
                    .query_pairs()
                    .any(|(key, value)| key == "name" && value == name)
        }));
    }
}

#[tokio::test]
async fn finds_drafts_on_later_pages_and_rejects_remote_tag_mismatch() {
    let server = MockServer::start().await;
    let mut other = release(&server, true, false);
    other["tag_name"] = json!("other-tag");
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/releases"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![other; 100]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/releases"))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([release(&server, true, false)])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/releases/42/assets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/example/demo/commits/demo-v1.2.3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": "different"})))
        .mount(&server)
        .await;
    let plan = plan();
    let directory = stage_all(&plan);
    let prepared = prepare(&plan, directory.path(), &repository()).unwrap();
    let error = publish(&github(&server), &prepared).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not match the built commit")
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method == "GET")
    );
}
