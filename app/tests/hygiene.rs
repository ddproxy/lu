//! Repository hygiene: retired product names, internal hosts and development node names must not
//! appear in tracked docs, sources or tests. Compatibility literals are listed in
//! `scripts/hygiene-allowlist.txt`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const BANNED: [&str; 9] = [
    "reiki",
    "toolgate",
    "nadi",
    "siglint",
    "worklog",
    "axiom",
    "code.ddproxy.net",
    "dev-ct-",
    "homelab",
];

const ALLOWLIST: &str = "scripts/hygiene-allowlist.txt";

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" || name == "hygiene-allowlist.txt" {
            continue;
        }
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if ty.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn allowlist(root: &Path) -> BTreeSet<(String, String)> {
    let text = fs::read_to_string(root.join(ALLOWLIST)).expect("read hygiene allowlist");
    let mut entries = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut columns = line.splitn(3, '\t');
        let path = columns.next().unwrap_or("").trim();
        let term = columns.next().unwrap_or("").trim().to_ascii_lowercase();
        let reason = columns.next().unwrap_or("").trim();
        assert!(
            !path.is_empty() && !term.is_empty() && !reason.is_empty(),
            "malformed allowlist line {}",
            index + 1
        );
        assert!(
            BANNED.contains(&term.as_str()),
            "unknown allowlist term on line {}",
            index + 1
        );
        assert!(
            entries.insert((path.to_owned(), term)),
            "duplicate allowlist entry on line {}",
            index + 1
        );
    }
    entries
}

#[test]
fn tracked_files_avoid_retired_names_and_internal_hosts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let mut files = Vec::new();
    collect_files(root, &mut files).expect("walk workspace");

    let mut findings: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .expect("file below workspace root")
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(&path).expect("read workspace file");
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (line_number, line) in text.lines().enumerate() {
            let lower = line.to_ascii_lowercase();
            for term in BANNED {
                if lower.contains(term) {
                    findings
                        .entry((relative.clone(), term.to_owned()))
                        .or_default()
                        .push(line_number + 1);
                }
            }
        }
    }

    let allowed = allowlist(root);
    let mut errors = Vec::new();
    for ((path, term), lines) in &findings {
        if !allowed.contains(&(path.clone(), term.clone())) {
            errors.push(format!("{path}:{} contains banned '{term}'", lines[0]));
        }
    }
    let seen: BTreeSet<(String, String)> = findings.keys().cloned().collect();
    for (path, term) in allowed.difference(&seen) {
        errors.push(format!("unused allowlist entry: {path} ({term})"));
    }
    assert!(
        errors.is_empty(),
        "repository hygiene failed:\n{}",
        errors.join("\n")
    );
}
