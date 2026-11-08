mod support;

use prview::git::git_cmd;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;
use tempfile::TempDir;

const IDENTITY_FIELDS: [&str; 5] = [
    "git_sha",
    "binary_build_id",
    "binary_build_id_status",
    "binary_build_id_reason",
    "release_tag",
];

fn git(repo: &Path, args: &[&str]) {
    let output = git_cmd().current_dir(repo).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(label: &str) -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    git(repo, &["init"]);
    git(repo, &["config", "user.name", "Test User"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    fs::write(repo.join("README.md"), format!("{label}\n")).unwrap();
    git(repo, &["add", "README.md"]);
    git(repo, &["commit", "-m", "initial"]);
    git(repo, &["branch", "-M", "main"]);
    git(repo, &["checkout", "-b", "feature/generator-identity"]);
    fs::write(repo.join("README.md"), format!("{label}\nupdated\n")).unwrap();
    git(repo, &["add", "README.md"]);
    git(repo, &["commit", "-m", "change"]);
    temp
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn generate_pack(repo: &Path, pack: &Path, spoof: bool) -> Value {
    let environment = support::ContractEnvironment::new();
    let mut command = environment.command();
    command.current_dir(repo).args([
        "--quick",
        "--json",
        "--quiet",
        "--no-heuristics",
        "--no-fetch",
        "--output-dir",
    ]);
    command.arg(pack).args(["HEAD", "main"]);
    if spoof {
        command
            .env(
                "PRVIEW_SOURCE_SHA",
                "ffffffffffffffffffffffffffffffffffffffff",
            )
            .env(
                "PRVIEW_BUILD_SOURCE_SHA",
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            )
            .env("PRVIEW_RELEASE_TAG", "v999.999.999")
            .env("PRVIEW_GENERATOR_RELEASE_TAG", "v999.999.999")
            .env("PRVIEW_BUILD_RELEASE_TAG", "v999.999.999");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "prview failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    read_json(&pack.join("report.json"))
}

#[test]
fn generator_identity_survives_runtime_spoof_reviewed_checkout_and_zip_export() {
    let first = fixture("first reviewed repository");
    let second = fixture("second reviewed repository");
    let exports = tempfile::tempdir().unwrap();
    let first_pack = exports.path().join("first");
    let second_pack = exports.path().join("second");
    let report = generate_pack(first.path(), &first_pack, false);
    let spoofed_report = generate_pack(second.path(), &second_pack, true);
    let runner = read_json(&first_pack.join("00_summary/RUN.json"));
    let spoofed_runner = read_json(&second_pack.join("00_summary/RUN.json"));
    let tool = &report["meta"]["tool"];

    let executable = fs::read(env!("CARGO_BIN_EXE_prview")).unwrap();
    assert_eq!(
        tool["binary_build_id"],
        format!("sha256:{:x}", Sha256::digest(executable))
    );
    assert_eq!(tool["binary_build_id_status"], "available");
    assert!(tool["binary_build_id_reason"].is_null());
    assert_eq!(tool["git_sha"], env!("PRVIEW_BUILD_SOURCE_SHA"));
    assert_eq!(
        tool["release_tag"],
        serde_json::json!(option_env!("PRVIEW_BUILD_RELEASE_TAG").filter(|tag| !tag.is_empty()))
    );
    for field in IDENTITY_FIELDS {
        assert_eq!(tool[field], runner["runner"][field], "RUN {field}");
        assert_eq!(
            tool[field], spoofed_report["meta"]["tool"][field],
            "spoof {field}"
        );
        assert_eq!(
            tool[field], spoofed_runner["runner"][field],
            "spoof RUN {field}"
        );
    }
    assert_ne!(
        runner["refs"]["target_sha"],
        spoofed_runner["refs"]["target_sha"]
    );
    assert_eq!(report["schema_version"], "3.2");
    assert_eq!(runner["schema_version"], "1.0");

    let human = fs::read_to_string(first_pack.join("00_summary/system_meta.txt")).unwrap();
    for field in IDENTITY_FIELDS {
        let expected = tool[field].as_str().unwrap_or(match field {
            "binary_build_id_reason" => "none",
            "release_tag" => "unknown",
            _ => "unavailable",
        });
        assert!(
            human
                .lines()
                .any(|line| line == format!("generator_{field}: {expected}"))
        );
    }

    // Inspect a copied archive: consumers need no original checkout or pack.
    let copied_zip = exports.path().join("copied-artifacts.zip");
    fs::copy(first_pack.join("artifacts.zip"), &copied_zip).unwrap();
    let mut archive = zip::ZipArchive::new(fs::File::open(copied_zip).unwrap()).unwrap();
    let manifest = read_json(&first_pack.join("00_summary/MANIFEST.json"));
    for relative in [
        "report.json",
        "00_summary/RUN.json",
        "00_summary/system_meta.txt",
    ] {
        let mut bytes = Vec::new();
        archive
            .by_name(relative)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(
            bytes,
            fs::read(first_pack.join(relative)).unwrap(),
            "ZIP {relative}"
        );
        let entry = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"].as_str().unwrap().replace('\\', "/") == relative)
            .unwrap();
        assert_eq!(entry["sha256"], format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(entry["size"], bytes.len());
    }
}
