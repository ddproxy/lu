//! Both shipped binaries print the full workspace build version, and the release script writes
//! that exact version into the build.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn binaries_print_the_full_build_version() {
    let version = env!("CARGO_PKG_VERSION");
    for (binary, name) in [
        (env!("CARGO_BIN_EXE_lu"), "lu"),
        (env!("CARGO_BIN_EXE_ting"), "ting"),
    ] {
        let output = Command::new(binary)
            .arg("--version")
            .output()
            .expect("run --version");
        assert!(
            output.status.success(),
            "{name} --version: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)
                .expect("utf-8 version output")
                .trim(),
            format!("{name} {version}")
        );
    }
}

#[test]
fn release_script_source_accepts_build_metadata() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let source = fs::read_to_string(root.join("scripts/set-release-version.py"))
        .expect("read release script");
    assert!(
        source.contains(r"(\+[0-9A-Za-z.-]+)?"),
        "script no longer accepts a +HASH dev version"
    );
}

#[test]
fn a_fake_dev_version_keeps_its_counter_and_hash_when_displayed() {
    let version = lu_proto::BuildVersion::parse("v0.6.2-dev.1.gc08a5a0").expect("fake dev version");
    assert!(version.is_dev());
    assert_eq!(version.to_string(), "0.6.2-dev.1.gc08a5a0");
}

/// CI has Python; local checks may not. Return the interpreter when one is available.
fn python() -> Option<&'static str> {
    ["python3", "python"]
        .into_iter()
        .find(|name| Command::new(name).arg("--version").output().is_ok())
}

/// Runs the release script on a throwaway checkout so the workspace manifest is never touched.
fn release_script(python: &str, tag: &str) -> (std::process::Output, String, String) {
    let dir = tempfile::tempdir().expect("temporary checkout");
    let scripts = dir.path().join("scripts");
    fs::create_dir(&scripts).expect("scripts directory");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    fs::copy(
        root.join("scripts/set-release-version.py"),
        scripts.join("set-release-version.py"),
    )
    .expect("copy release script");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.6.1\"\n",
    )
    .expect("write manifest");
    fs::write(
        dir.path().join("Cargo.lock"),
        "[[package]]\nname = \"lu\"\nversion = \"0.6.1\"\n\n\
         [[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n\
         source = \"registry+https://example.invalid/index\"\n",
    )
    .expect("write lockfile");
    let output = Command::new(python)
        .arg(scripts.join("set-release-version.py"))
        .arg(tag)
        .output()
        .expect("run release script");
    let manifest = fs::read_to_string(dir.path().join("Cargo.toml")).expect("read manifest");
    let lock = fs::read_to_string(dir.path().join("Cargo.lock")).expect("read lockfile");
    (output, manifest, lock)
}

#[test]
fn release_script_writes_fake_dev_versions_into_manifest_and_lockfile() {
    let Some(python) = python() else {
        eprintln!("no Python interpreter; skipping release script execution");
        return;
    };
    for tag in ["v0.6.2-dev.1.gc08a5a0", "v0.6.2-dev.1+gc08a5a0"] {
        let version = &tag[1..];
        let (output, manifest, lock) = release_script(python, tag);
        assert!(
            output.status.success(),
            "{tag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            manifest.contains(&format!("version = \"{version}\"")),
            "{tag}: {manifest}"
        );
        assert!(
            lock.contains(&format!("version = \"{version}\"")),
            "{tag}: {lock}"
        );
        assert!(lock.contains("version = \"1.0.0\""), "{tag}: {lock}");
    }
}

#[test]
fn release_script_refuses_a_stable_tag_that_does_not_match() {
    let Some(python) = python() else {
        eprintln!("no Python interpreter; skipping release script execution");
        return;
    };
    let (output, manifest, _) = release_script(python, "v0.6.7");
    assert!(!output.status.success());
    assert!(manifest.contains("version = \"0.6.1\""), "{manifest}");
}

#[test]
fn release_script_refuses_a_stable_tag_with_a_dash_in_its_metadata() {
    let Some(python) = python() else {
        eprintln!("no Python interpreter; skipping release script execution");
        return;
    };
    let (output, manifest, _) = release_script(python, "v0.6.7+a-b");
    assert!(!output.status.success());
    assert!(manifest.contains("version = \"0.6.1\""), "{manifest}");
}
