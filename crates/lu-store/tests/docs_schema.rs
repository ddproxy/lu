//! Keeps the prose about the database schema in step with the code: a sentence that names "schema
//! version N" as the current one must use `SCHEMA_VERSION`.

use lu_store::SCHEMA_VERSION;
use std::path::Path;

/// Words that mark a sentence as history or migration rather than a statement of the current schema.
const HISTORICAL: [&str; 6] = ["migrat", "older", "from ", "gain", "previous", "bump"];

fn mentions(line: &str) -> Vec<i64> {
    let lower = line.to_ascii_lowercase();
    let mut found = Vec::new();
    for marker in [
        "schema version ",
        "schema_version\": ",
        "baseline (currently version ",
    ] {
        let mut rest = lower.as_str();
        while let Some(at) = rest.find(marker) {
            let after = rest[at + marker.len()..].trim_start_matches(['`', '"']);
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse() {
                found.push(n);
            }
            rest = &rest[at + marker.len()..];
        }
    }
    found
}

#[test]
fn docs_name_the_current_schema_version() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let docs = root.join("docs");
    let mut paths: Vec<_> = std::fs::read_dir(&docs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .collect();
    paths.push(root.join("README.md"));
    paths.push(root.join("llms.txt"));
    let mut stale = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        for (number, line) in text.lines().enumerate() {
            // Judge each sentence on its own, so a migration remark in one sentence does not
            // excuse a stale statement of the current schema in the next.
            for sentence in line.split(". ") {
                let lower = sentence.to_ascii_lowercase();
                if HISTORICAL.iter().any(|word| lower.contains(word)) {
                    continue;
                }
                for version in mentions(sentence) {
                    if version != SCHEMA_VERSION {
                        stale.push(format!(
                            "{}:{}: says schema version {version}, code is {SCHEMA_VERSION}",
                            path.file_name().unwrap().to_string_lossy(),
                            number + 1
                        ));
                    }
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "stale schema statements:\n{}",
        stale.join("\n")
    );
}
