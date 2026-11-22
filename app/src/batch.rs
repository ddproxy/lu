//! Shared batch planning and submission for `lu submit --batch` and `lu_submit_batch`.
//!
//! Nothing here connects to a controller while printing a plan. Every input error is collected
//! before the first submission, and every job carries a caller-supplied idempotency key, so a
//! repeated batch is a no-op rather than a second set of jobs.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use lu_client::{Client, JobView};
use lu_proto::{
    printable, valid_group, validate_idempotency_key, ExpectMode, ExpectSpec, JobSpec,
    WorkspacePolicy, DEFAULT_PRIORITY, DEFAULT_TIMEOUT_SECS, MAX_AFTER_JOBS, MAX_GROUP_BYTES,
    MAX_GROUP_RUNNING, MAX_PRIORITY, MAX_TASK_BYTES, MAX_TIMEOUT_SECS, MIN_TIMEOUT_SECS,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Task files and checklists are bounded independently of the controller's task cap so one bad
/// input cannot make the operator allocate without limit.
const CONFIG_LIMIT: usize = 64 * 1024;
const CHECKLIST_LIMIT: usize = 1024 * 1024;
const SLUG_BYTES: usize = 64;

pub struct BatchOptions<'a> {
    pub path: &'a Path,
    pub key_prefix: &'a str,
    pub defaults: Option<&'a Path>,
    pub repo: Option<&'a str>,
    pub git_ref: Option<&'a str>,
    pub chain: bool,
    pub max_parallel: Option<usize>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectFields {
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    changed: Vec<String>,
    #[serde(default)]
    forbid_changed: Vec<String>,
    #[serde(default)]
    min_diff_bytes: Option<u64>,
    #[serde(default)]
    max_diff_bytes: Option<u64>,
    #[serde(default)]
    mode: Option<String>,
}

impl ExpectFields {
    fn build(self) -> Result<ExpectSpec, String> {
        let mode = self
            .mode
            .as_deref()
            .map(ExpectMode::parse)
            .transpose()
            .map_err(|error| format!("expect.mode: {error}"))?
            .unwrap_or_default();
        Ok(ExpectSpec {
            files: self.files,
            changed: self.changed,
            forbid_changed: self.forbid_changed,
            min_diff_bytes: self.min_diff_bytes,
            max_diff_bytes: self.max_diff_bytes,
            mode,
        })
    }
}

/// The fields a sidecar may set. `Option<Vec<_>>` lets an empty array clear a default.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedFields {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    expect: Option<ExpectFields>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    require: Option<Vec<String>>,
    #[serde(default)]
    after_keys: Option<Vec<String>>,
}

impl SharedFields {
    /// Sidecar values win over defaults; a present empty array clears the default list.
    fn overlay(mut self, other: SharedFields) -> Self {
        macro_rules! take {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field;
                }
            };
        }
        take!(provider);
        take!(model);
        take!(priority);
        take!(group);
        take!(timeout);
        take!(expect);
        take!(workspace);
        take!(require);
        take!(after_keys);
        self
    }

    fn resolve(self) -> Result<ResolvedFields, String> {
        if let Some(priority) = self.priority {
            if priority > MAX_PRIORITY {
                return Err(format!(
                    "priority must be 0 to {MAX_PRIORITY} (got {priority})"
                ));
            }
        }
        if let Some(group) = &self.group {
            if !valid_group(group) {
                return Err(format!("group {group:?} must use A-Z a-z 0-9 . _ -"));
            }
        }
        if let Some(timeout) = self.timeout {
            if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout) {
                return Err(format!(
                    "timeout must be {MIN_TIMEOUT_SECS} to {MAX_TIMEOUT_SECS} seconds (got {timeout})"
                ));
            }
        }
        let workspace = self
            .workspace
            .as_deref()
            .map(WorkspacePolicy::parse)
            .transpose()
            .map_err(|error| error.to_string())?;
        let expect = self.expect.map(ExpectFields::build).transpose()?;
        Ok(ResolvedFields {
            provider: self.provider,
            model: self.model,
            priority: self.priority.unwrap_or(DEFAULT_PRIORITY),
            group: self.group,
            timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS),
            expect,
            workspace,
            require: self.require.unwrap_or_default(),
            after_keys: self.after_keys.unwrap_or_default(),
        })
    }
}

struct ResolvedFields {
    provider: Option<String>,
    model: Option<String>,
    priority: u8,
    group: Option<String>,
    timeout: u64,
    expect: Option<ExpectSpec>,
    workspace: Option<WorkspacePolicy>,
    require: Vec<String>,
    after_keys: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultsFile {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    expect: Option<ExpectFields>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    require: Option<Vec<String>>,
    #[serde(default)]
    after_keys: Option<Vec<String>>,
}

impl DefaultsFile {
    fn shared(&self) -> SharedFields {
        SharedFields {
            provider: self.provider.clone(),
            model: self.model.clone(),
            priority: self.priority,
            group: self.group.clone(),
            timeout: self.timeout,
            expect: self.expect.clone(),
            workspace: self.workspace.clone(),
            require: self.require.clone(),
            after_keys: self.after_keys.clone(),
        }
    }
}

struct RawJob {
    slug_hint: String,
    parts: Vec<String>,
    source: String,
    sidecar: SharedFields,
}

impl RawJob {
    fn task_and_hints(&self) -> (String, Vec<String>) {
        let joined = self.parts.join("\n");
        let mut task = String::new();
        let mut hints = Vec::new();
        for line in joined.lines() {
            match line.find("after:") {
                Some(at) => {
                    task.push_str(line[..at].trim_end());
                    hints.push(line[at + "after:".len()..].trim().to_owned());
                }
                None => task.push_str(line),
            }
            task.push('\n');
        }
        (task.trim().to_owned(), hints)
    }
}

pub struct PlannedJob {
    slug: String,
    key: String,
    spec: JobSpec,
    hints: Vec<String>,
    after_names: Vec<String>,
    after_keys: Vec<String>,
    after_display: Vec<String>,
    source: String,
}

pub struct BatchPlan {
    jobs: Vec<PlannedJob>,
    group_cap: Option<(String, usize)>,
    chain: bool,
}

fn push_error(errors: &mut Vec<String>, source: &str, error: impl std::fmt::Display) {
    errors.push(format!("{source}: {}", printable(&error.to_string())));
}

fn read_limited(path: &Path, limit: usize, label: &str) -> Result<String, String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot read {label} {}: {error}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("refusing symlinked {label} {}", path.display()));
    }
    if !meta.is_file() {
        return Err(format!("{label} {} is not a regular file", path.display()));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("cannot read {label} {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {label} {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "{label} {} is larger than {limit} bytes",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{label} {} is not valid UTF-8", path.display()))
}

fn load_toml<T: DeserializeOwned>(path: &Path, required: bool) -> Result<Option<T>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if required {
                return Err(format!("cannot read {}: {error}", path.display()));
            }
            return Ok(None);
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    }
    let text = read_limited(path, CONFIG_LIMIT, "config")?;
    let value = toml::from_str(&text)
        .map_err(|error| format!("{}: {}", path.display(), printable(&error.to_string())))?;
    Ok(Some(value))
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            separator = false;
        } else if matches!(ch, '.' | '_' | '-') {
            if !out.is_empty() && !separator {
                out.push(ch);
                separator = true;
            }
        } else if !out.is_empty() && !separator {
            out.push('-');
            separator = true;
        }
    }
    while out.ends_with(['.', '_', '-']) {
        out.pop();
    }
    while out.len() > SLUG_BYTES {
        out.pop();
    }
    while out.ends_with(['.', '_', '-']) {
        out.pop();
    }
    out
}

fn sidecar_for(path: &Path) -> PathBuf {
    path.with_extension("toml")
}

fn checklist_marker(line: &str) -> Option<(usize, bool, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    if let Some(text) = rest.strip_prefix("- [ ] ") {
        return Some((indent, false, text));
    }
    for prefix in ["- [x] ", "- [X] "] {
        if let Some(text) = rest.strip_prefix(prefix) {
            return Some((indent, true, text));
        }
    }
    None
}

fn parse_checklist(text: &str, path: &Path, sidecar: SharedFields) -> Vec<RawJob> {
    let mut jobs = Vec::new();
    let mut fence: Option<&str> = None;
    let mut base_indent: Option<usize> = None;
    let mut current: Option<usize> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(marker) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
            fence = if fence == Some(marker) {
                None
            } else {
                Some(marker)
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some((indent, checked, text)) = checklist_marker(line) {
            let top_level = match base_indent {
                Some(base) => indent <= base,
                None => true,
            };
            if top_level {
                base_indent = Some(indent);
                if checked {
                    current = None;
                } else {
                    jobs.push(RawJob {
                        slug_hint: text.to_owned(),
                        parts: vec![text.to_owned()],
                        source: format!("{}:{}", path.display(), index + 1),
                        sidecar: sidecar.clone(),
                    });
                    current = Some(jobs.len() - 1);
                }
            } else if !checked {
                if let Some(at) = current {
                    jobs[at].parts.push(trimmed.to_owned());
                }
            }
            continue;
        }
        let indent = line.len() - trimmed.len();
        if let Some(at) = current {
            if base_indent.is_some_and(|base| indent > base) || trimmed.is_empty() {
                jobs[at].parts.push(trimmed.to_owned());
            } else {
                current = None;
            }
        }
    }
    jobs
}

fn read_inputs(opts: &BatchOptions<'_>, errors: &mut Vec<String>) -> Vec<RawJob> {
    let meta = match fs::symlink_metadata(opts.path) {
        Ok(meta) => meta,
        Err(error) => {
            push_error(
                errors,
                "batch",
                format!("cannot read {}: {error}", opts.path.display()),
            );
            return Vec::new();
        }
    };
    if meta.file_type().is_symlink() {
        push_error(
            errors,
            "batch",
            format!("refusing symlinked batch path {}", opts.path.display()),
        );
        return Vec::new();
    }
    if meta.is_dir() {
        let mut entries = match fs::read_dir(opts.path) {
            Ok(entries) => entries.collect::<Vec<_>>(),
            Err(error) => {
                push_error(
                    errors,
                    "batch",
                    format!("cannot read {}: {error}", opts.path.display()),
                );
                return Vec::new();
            }
        };
        entries.sort_by_key(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
        });
        let mut jobs = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_error(errors, "batch", error);
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext == "toml")
            {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    push_error(errors, &name, error);
                    continue;
                }
            };
            if file_type.is_symlink() {
                push_error(errors, &name, "refusing symlinked batch file");
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let stem = path
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let task = match read_limited(&path, MAX_TASK_BYTES, "task") {
                Ok(task) => task,
                Err(error) => {
                    push_error(errors, &name, error);
                    continue;
                }
            };
            let sidecar = match load_toml::<SharedFields>(&sidecar_for(&path), false) {
                Ok(sidecar) => sidecar.unwrap_or_default(),
                Err(error) => {
                    push_error(errors, &name, error);
                    continue;
                }
            };
            jobs.push(RawJob {
                slug_hint: stem,
                parts: vec![task],
                source: path.display().to_string(),
                sidecar,
            });
        }
        return jobs;
    }
    if !meta.is_file() {
        push_error(
            errors,
            "batch",
            format!("{} is not a file or directory", opts.path.display()),
        );
        return Vec::new();
    }
    if opts.path.extension().is_some_and(|ext| ext == "toml") {
        push_error(
            errors,
            "batch",
            format!("{} is a TOML config, not a checklist", opts.path.display()),
        );
        return Vec::new();
    }
    let text = match read_limited(opts.path, CHECKLIST_LIMIT, "checklist") {
        Ok(text) => text,
        Err(error) => {
            push_error(errors, "checklist", error);
            return Vec::new();
        }
    };
    let sidecar = match load_toml::<SharedFields>(&sidecar_for(opts.path), false) {
        Ok(sidecar) => sidecar.unwrap_or_default(),
        Err(error) => {
            push_error(errors, "checklist", error);
            return Vec::new();
        }
    };
    parse_checklist(&text, opts.path, sidecar)
}

fn prefix_group(prefix: &str) -> String {
    let group = slugify(prefix);
    if group.is_empty() || !valid_group(&group) {
        "batch".to_owned()
    } else {
        group
    }
}

/// Builds a complete plan or returns every input error. No controller connection is used here.
pub fn plan(opts: &BatchOptions<'_>) -> Result<BatchPlan, Vec<String>> {
    let mut errors = Vec::new();
    if let Err(error) = validate_idempotency_key(opts.key_prefix) {
        push_error(&mut errors, "key_prefix", error);
    }
    if let Some(max) = opts.max_parallel {
        if !(1..=MAX_GROUP_RUNNING).contains(&max) {
            push_error(
                &mut errors,
                "max_parallel",
                format!("must be 1 to {MAX_GROUP_RUNNING} (got {max})"),
            );
        }
    }
    let defaults = match opts.defaults {
        Some(path) => match load_toml::<DefaultsFile>(path, true) {
            Ok(defaults) => defaults,
            Err(error) => {
                push_error(&mut errors, "defaults", error);
                None
            }
        },
        None => None,
    };
    let repo = opts
        .repo
        .map(str::to_owned)
        .or_else(|| defaults.as_ref().and_then(|file| file.repo.clone()));
    let git_ref = opts
        .git_ref
        .map(str::to_owned)
        .or_else(|| defaults.as_ref().and_then(|file| file.git_ref.clone()));
    if repo.as_deref().is_none_or(str::is_empty) {
        push_error(&mut errors, "repo", "is required for a batch");
    }
    if git_ref.as_deref().is_none_or(str::is_empty) {
        push_error(&mut errors, "ref", "is required for a batch");
    }
    let shared_defaults = defaults
        .as_ref()
        .map(DefaultsFile::shared)
        .unwrap_or_default();
    let raw_jobs = read_inputs(opts, &mut errors);
    if raw_jobs.is_empty() && errors.is_empty() {
        errors.push("batch produced no jobs".to_owned());
    }

    let mut jobs = Vec::new();
    let mut slugs = HashSet::new();
    for raw in raw_jobs {
        let (task, hints) = raw.task_and_hints();
        if task.is_empty() {
            push_error(&mut errors, &raw.source, "task is empty");
        }
        if task.len() > MAX_TASK_BYTES {
            push_error(
                &mut errors,
                &raw.source,
                format!("task is larger than {MAX_TASK_BYTES} bytes"),
            );
        }
        let slug = slugify(&raw.slug_hint);
        if slug.is_empty() {
            push_error(&mut errors, &raw.source, "cannot derive a job slug");
        }
        if !slugs.insert(slug.clone()) {
            push_error(
                &mut errors,
                &raw.source,
                format!("duplicate job key {slug:?}"),
            );
        }
        let key = format!("{}-{}", opts.key_prefix, slug);
        if let Err(error) = validate_idempotency_key(&key) {
            push_error(&mut errors, &raw.source, error);
        }
        let fields = shared_defaults
            .clone()
            .overlay(raw.sidecar.clone())
            .resolve();
        let Ok(fields) = fields else {
            // `resolve` already reports the offending field; add the source without its text.
            errors.push(format!("{}: invalid batch settings", raw.source));
            continue;
        };
        let spec = JobSpec {
            source: lu_proto::SourceSpec {
                repo: repo.clone().unwrap_or_default(),
                git_ref: git_ref.clone().unwrap_or_default(),
                base: None,
            },
            repo: repo.clone().unwrap_or_default(),
            git_ref: git_ref.clone().unwrap_or_default(),
            base_commit: None,
            task,
            harness: "opencode".to_owned(),
            provider: fields.provider,
            priority: fields.priority,
            group: fields.group,
            allow_spawn: false,
            spawn_inherit: false,
            spawn_repos: Vec::new(),
            workspace: fields.workspace,
            cache_dirs: Vec::new(),
            context: None,
            origin: None,
            origin_ref: None,
            untrusted_context: false,
            draft: false,
            publish: None,
            model: fields.model,
            timeout_secs: fields.timeout,
            idle_timeout_secs: None,
            on_stuck: None,
            stuck_after_secs: None,
            idempotency_key: Some(key.clone()),
            after: Vec::new(),
            collect: Default::default(),
            include_failed: false,
            on_conflict: Default::default(),
            allow_empty: false,
            requires: fields.require,
            tolerates: Vec::new(),
            fallbacks: Vec::new(),
            expect: fields.expect,
        };
        if let Err(error) = spec.validate() {
            push_error(&mut errors, &raw.source, error);
        }
        jobs.push(PlannedJob {
            slug,
            key,
            spec,
            hints: {
                let mut all = fields.after_keys;
                all.extend(hints);
                all
            },
            after_names: Vec::new(),
            after_keys: Vec::new(),
            after_display: Vec::new(),
            source: raw.source,
        });
    }

    let slugs: HashMap<String, usize> = jobs
        .iter()
        .enumerate()
        .map(|(at, job)| (job.slug.clone(), at))
        .collect();
    let keys: HashMap<String, usize> = jobs
        .iter()
        .enumerate()
        .map(|(at, job)| (job.key.clone(), at))
        .collect();
    for at in 0..jobs.len() {
        let hints = jobs[at].hints.clone();
        for hint in split_keys(&hints) {
            let Some(target) = slugs.get(&hint).or_else(|| keys.get(&hint)).copied() else {
                if let Err(error) = validate_idempotency_key(&hint) {
                    push_error(&mut errors, &jobs[at].source, error);
                }
                jobs[at].after_keys.push(hint.clone());
                jobs[at].after_display.push(hint);
                continue;
            };
            if target >= at {
                push_error(
                    &mut errors,
                    &jobs[at].source,
                    format!(
                        "after key {:?} names a later or current job",
                        printable(&hint)
                    ),
                );
            }
            let target_slug = jobs[target].slug.clone();
            let target_key = jobs[target].key.clone();
            push_unique(&mut jobs[at].after_names, target_slug);
            push_unique(&mut jobs[at].after_display, target_key);
        }
        if opts.chain {
            for previous in 0..at {
                let previous_slug = jobs[previous].slug.clone();
                let previous_key = jobs[previous].key.clone();
                push_unique(&mut jobs[at].after_names, previous_slug);
                push_unique(&mut jobs[at].after_display, previous_key);
            }
        }
        let count = jobs[at].after_names.len() + jobs[at].after_keys.len();
        if count > MAX_AFTER_JOBS {
            push_error(
                &mut errors,
                &jobs[at].source,
                format!("after has {count} keys, at most {MAX_AFTER_JOBS}"),
            );
        }
    }

    let group_cap = if let Some(max) = opts.max_parallel {
        let grouped = jobs.iter().filter(|job| job.spec.group.is_some()).count();
        let distinct: HashSet<&str> = jobs
            .iter()
            .filter_map(|job| job.spec.group.as_deref())
            .collect();
        if grouped != 0 && grouped != jobs.len() {
            push_error(
                &mut errors,
                "max_parallel",
                "every job must set the same group when a cap is requested",
            );
        }
        if distinct.len() > 1 {
            push_error(
                &mut errors,
                "max_parallel",
                "jobs use different groups; a cap needs one shared group",
            );
        }
        let group = distinct
            .iter()
            .next()
            .map(|name| (*name).to_owned())
            .unwrap_or_else(|| prefix_group(opts.key_prefix));
        if !valid_group(&group) || group.len() > MAX_GROUP_BYTES {
            push_error(&mut errors, "max_parallel", "derived group name is invalid");
        }
        for job in &mut jobs {
            job.spec.group = Some(group.clone());
        }
        Some((group, max))
    } else {
        None
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(BatchPlan {
        jobs,
        group_cap,
        chain: opts.chain,
    })
}

fn split_keys(values: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        for part in value.split(|ch: char| ch == ',' || ch.is_whitespace()) {
            if !part.is_empty() {
                out.push(part.to_owned());
            }
        }
    }
    out
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

impl BatchPlan {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn entries(&self) -> Vec<SubmissionEntry> {
        self.jobs
            .iter()
            .map(|job| SubmissionEntry {
                label: job.slug.clone(),
                after_names: job.after_names.clone(),
                after_keys: job.after_keys.clone(),
                spec: job.spec.clone(),
            })
            .collect()
    }

    fn group_cap(&self) -> Option<(&str, usize)> {
        self.group_cap
            .as_ref()
            .map(|(group, max)| (group.as_str(), *max))
    }
}

/// A stable, JSON-safe shape used by both the CLI and the MCP dry run.
pub fn plan_structured(plan: &BatchPlan) -> serde_json::Value {
    let jobs = plan
        .jobs
        .iter()
        .map(|job| {
            serde_json::json!({
                "key": job.key,
                "slug": job.slug,
                "group": job.spec.group,
                "after": job.after_display,
                "bytes": job.spec.task.len(),
                "source": printable(&job.source),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "dry_run": true,
        "chain": plan.chain,
        "max_parallel": plan.group_cap.as_ref().map(|(_, max)| max),
        "group": plan.group_cap.as_ref().map(|(group, _)| group),
        "jobs": jobs,
    })
}

pub fn print_plan(plan: &BatchPlan, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&plan_structured(plan)).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let rows = plan
        .jobs
        .iter()
        .enumerate()
        .map(|(at, job)| {
            vec![
                (at + 1).to_string(),
                job.key.clone(),
                job.spec.group.clone().unwrap_or_else(|| "-".to_owned()),
                if job.after_display.is_empty() {
                    "-".to_owned()
                } else {
                    job.after_display.join(",")
                },
                job.spec.task.len().to_string(),
                printable(&job.source),
            ]
        })
        .collect();
    crate::operator::print_table(&["#", "KEY", "GROUP", "AFTER", "BYTES", "SOURCE"], rows);
    println!(
        "{} job(s); chain={}; max_parallel={}",
        plan.jobs.len(),
        plan.chain,
        plan.group_cap
            .as_ref()
            .map(|(_, max)| max.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    Ok(())
}

/// A submission before external keys and in-batch labels are resolved to job ids.
pub struct SubmissionEntry {
    pub label: String,
    pub after_names: Vec<String>,
    pub after_keys: Vec<String>,
    pub spec: JobSpec,
}

/// A submission whose dependencies are ready to be attached to its spec.
pub struct ReadyEntry {
    pub label: String,
    pub after_names: Vec<String>,
    pub spec: JobSpec,
}

#[derive(Debug, Clone)]
pub struct SubmitOutcome {
    pub label: String,
    pub key: String,
    pub view: JobView,
}

#[derive(Debug)]
pub struct SubmitFailure {
    pub reason: String,
    pub submitted: Vec<SubmitOutcome>,
    pub not_submitted: Vec<String>,
    /// Preflight failures happen before the first request and are usage errors in the CLI.
    pub preflight: bool,
}

impl SubmitFailure {
    fn new(
        reason: impl Into<String>,
        submitted: Vec<SubmitOutcome>,
        not_submitted: Vec<String>,
    ) -> Self {
        Self {
            reason: printable(&reason.into()),
            submitted,
            not_submitted,
            preflight: false,
        }
    }

    fn preflight(reason: impl Into<String>, labels: Vec<String>) -> Self {
        Self {
            reason: printable(&reason.into()),
            submitted: Vec::new(),
            not_submitted: labels,
            preflight: true,
        }
    }
}

/// Hashes a spec into the same stable key used by `lu_submit` and `lu_submit_many`.
pub fn derived_key(spec: &JobSpec) -> String {
    let mut plain = spec.clone();
    plain.idempotency_key = None;
    let digest = Sha256::digest(plain.canonical_json().as_bytes());
    let mut key = String::from("mcp-");
    for byte in &digest[..8] {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}

/// Resolves external idempotency keys before anything is submitted.
pub async fn resolve_entries(
    client: &Client,
    entries: Vec<SubmissionEntry>,
) -> Result<Vec<ReadyEntry>, SubmitFailure> {
    let labels = entries
        .iter()
        .map(|entry| entry.label.clone())
        .collect::<Vec<_>>();
    let mut errors = Vec::new();
    let mut ready = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut spec = entry.spec;
        for key in &entry.after_keys {
            match client.job_by_key(key).await {
                Ok(Some(view)) if !spec.after.contains(&view.id) => spec.after.push(view.id),
                Ok(Some(_)) => {}
                Ok(None) => errors.push(format!(
                    "{}: no job with idempotency key {:?}",
                    entry.label,
                    printable(key)
                )),
                Err(error) => errors.push(format!("{}: {}", entry.label, error)),
            }
        }
        ready.push(ReadyEntry {
            label: entry.label,
            after_names: entry.after_names,
            spec,
        });
    }
    if !errors.is_empty() {
        return Err(SubmitFailure::preflight(errors.join("; "), labels));
    }
    Ok(ready)
}

pub struct PreparedBatch {
    entries: Vec<ReadyEntry>,
}

impl PreparedBatch {
    /// Wraps already-resolved entries for callers that do not need a group cap.
    pub fn new(entries: Vec<ReadyEntry>) -> Self {
        Self { entries }
    }
}

pub async fn prepare(client: &Client, plan: &BatchPlan) -> Result<PreparedBatch, SubmitFailure> {
    let entries = resolve_entries(client, plan.entries()).await?;
    if let Some((group, max)) = plan.group_cap() {
        client.set_group_limit(group, max).await.map_err(|error| {
            SubmitFailure::preflight(
                format!("cannot set group {group} limit to {max}: {error}"),
                entries.iter().map(|entry| entry.label.clone()).collect(),
            )
        })?;
    }
    Ok(PreparedBatch { entries })
}

pub async fn submit_prepared(
    client: &Client,
    prepared: &PreparedBatch,
) -> Result<Vec<SubmitOutcome>, SubmitFailure> {
    let entries = &prepared.entries;
    let mut ids = HashMap::new();
    let mut submitted = Vec::new();
    for (at, entry) in entries.iter().enumerate() {
        let mut spec = entry.spec.clone();
        for name in &entry.after_names {
            let Some(id) = ids.get(name.as_str()) else {
                return Err(SubmitFailure::new(
                    format!("{}: unknown earlier job name {:?}", entry.label, name),
                    submitted,
                    entries[at..]
                        .iter()
                        .map(|entry| entry.label.clone())
                        .collect(),
                ));
            };
            if !spec.after.contains(id) {
                spec.after.push(id.clone());
            }
        }
        if spec.idempotency_key.is_none() {
            spec.idempotency_key = Some(derived_key(&spec));
        }
        if let Err(error) = spec.validate() {
            return Err(SubmitFailure::new(
                format!("{}: {error}", entry.label),
                submitted,
                entries[at..]
                    .iter()
                    .map(|entry| entry.label.clone())
                    .collect(),
            ));
        }
        match client.submit(&spec).await {
            Ok(view) => {
                let key = spec
                    .idempotency_key
                    .clone()
                    .unwrap_or_else(|| derived_key(&spec));
                ids.insert(entry.label.clone(), view.id.clone());
                submitted.push(SubmitOutcome {
                    label: entry.label.clone(),
                    key,
                    view,
                });
            }
            Err(error) => {
                return Err(SubmitFailure::new(
                    format!("{}: {error}", entry.label),
                    submitted,
                    entries[at..]
                        .iter()
                        .map(|entry| entry.label.clone())
                        .collect(),
                ));
            }
        }
    }
    Ok(submitted)
}

/// The shared result shape for successful batch submissions, including partial failures.
pub fn outcomes_structured(
    outcomes: &[SubmitOutcome],
    not_submitted: &[String],
) -> serde_json::Value {
    let jobs = outcomes
        .iter()
        .map(|outcome| {
            serde_json::json!({
                "key": outcome.key,
                "name": printable(&outcome.label),
                "id": outcome.view.id.to_string(),
                "state": crate::operator::state_name(outcome.view.state),
                "after": outcome.view.after.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "jobs": jobs,
        "submitted_count": outcomes.len(),
        "not_submitted": not_submitted,
    })
}

pub fn print_outcomes(outcomes: &[SubmitOutcome], json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&outcomes_structured(outcomes, &[])).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let rows = outcomes
        .iter()
        .map(|outcome| {
            vec![
                outcome.key.clone(),
                outcome.view.id.to_string(),
                crate::operator::state_name(outcome.view.state).to_owned(),
                if outcome.view.after.is_empty() {
                    "-".to_owned()
                } else {
                    outcome.view.after.len().to_string()
                },
                printable(&outcome.label),
            ]
        })
        .collect();
    crate::operator::print_table(&["KEY", "ID", "STATE", "AFTER", "NAME"], rows);
    Ok(())
}
