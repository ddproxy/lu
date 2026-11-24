//! The TOML examples in `docs/scheduling.md` must stay parseable by the real config type.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Read;

const DOCS: &str = include_str!("../../../docs/scheduling.md");

fn runtime_hash() -> String {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .expect("open urandom")
        .read_exact(&mut bytes)
        .expect("read urandom");
    let mut out = String::with_capacity(64);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex");
    }
    out
}

fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.trim() == "```toml" {
            current = Some(String::new());
        } else if line.trim() == "```" {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
        } else if let Some(block) = &mut current {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
}

fn collect_keys(value: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match child {
                    toml::Value::Table(_) => collect_keys(child, &path, out),
                    toml::Value::Array(items)
                        if !items.is_empty()
                            && items
                                .iter()
                                .all(|item| matches!(item, toml::Value::Table(_))) =>
                    {
                        for item in items {
                            collect_keys(item, &format!("{path}[]"), out);
                        }
                    }
                    _ => {
                        out.insert(path);
                    }
                }
            }
        }
        _ => {
            out.insert(prefix.to_owned());
        }
    }
}

#[test]
fn scheduling_examples_are_valid_config() {
    let blocks = toml_blocks(DOCS);
    assert!(
        blocks.len() >= 2,
        "expected at least two TOML examples, found {}",
        blocks.len()
    );

    let allowed: BTreeSet<&str> = [
        "controller.listen",
        "controller.db",
        "controller.artifacts_dir",
        "controller.agents[].name",
        "controller.agents[].token_sha256",
        "controller.operators[].name",
        "controller.operators[].token_sha256",
        "controller.operators[].schedule",
        "agent.name",
        "agent.controller_url",
        "agent.token_file",
        "agent.work_dir",
        "agent.harness",
        "schedule.horizon_secs",
        "schedule.step_budget",
        "schedule.staging_secs",
        "schedule.finish_secs",
        "schedule.starvation_secs",
        "schedule.min_disk_free_pct",
        "calendar[].name",
        "calendar[].offset",
        "calendar[].default",
        "calendar[].windows[].id",
        "calendar[].windows[].kind",
        "calendar[].windows[].from",
        "calendar[].windows[].to",
        "calendar[].windows[].days",
        "attach[].scope",
        "attach[].calendars",
        "schedule.calendars[].name",
        "schedule.calendars[].offset",
        "schedule.calendars[].default",
        "schedule.calendars[].exceptions[].id",
        "schedule.calendars[].exceptions[].kind",
        "schedule.calendars[].exceptions[].from",
        "schedule.calendars[].exceptions[].to",
        "schedule.calendars[].exceptions[].reason",
        "schedule.attachments[].scope",
        "schedule.attachments[].name",
        "schedule.attachments[].calendars",
    ]
    .into_iter()
    .collect();

    let mut seen = BTreeSet::new();
    for (index, block) in blocks.iter().enumerate() {
        let value: toml::Value = toml::from_str(block).unwrap_or_else(|error| {
            panic!("scheduling example {} is not TOML: {error}", index + 1)
        });
        let mut keys = BTreeSet::new();
        collect_keys(&value, "", &mut keys);
        for key in &keys {
            assert!(
                allowed.contains(key.as_str()),
                "scheduling example {} has unexpected key {key}",
                index + 1
            );
        }
        seen.extend(keys);

        let text = block
            .replace("AGENT_TOKEN_HASH", &runtime_hash())
            .replace("OPERATOR_TOKEN_HASH", &runtime_hash());
        lu_config::Config::parse(&text)
            .unwrap_or_else(|error| panic!("scheduling example {} is invalid: {error}", index + 1));
    }

    let missing: Vec<_> = allowed.iter().filter(|key| !seen.contains(**key)).collect();
    assert!(
        missing.is_empty(),
        "scheduling examples no longer show: {missing:?}"
    );
}

#[test]
fn an_unknown_docs_key_is_rejected() {
    let block = &toml_blocks(DOCS)[0];
    let mut text = block
        .replace("AGENT_TOKEN_HASH", &runtime_hash())
        .replace("OPERATOR_TOKEN_HASH", &runtime_hash());
    text.push_str("\n[bogus_docs_key]\nvalue = 1\n");
    assert!(
        lu_config::Config::parse(&text).is_err(),
        "the config parser must reject unknown keys"
    );
}
