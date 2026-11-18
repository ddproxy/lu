//! The labels a node advertises. Fixed labels come from config; each file is re-read before
//! every heartbeat so an operator can change a node's offers without restarting it.

use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use lu_proto::{printable, valid_label, MAX_LABEL_FILE_BYTES, MAX_NODE_LABELS};
use tracing::warn;

pub struct LabelSource {
    fixed: Vec<String>,
    files: Vec<String>,
    /// Keys already warned about, so a bad line does not fill the log every heartbeat.
    warned: Mutex<HashSet<String>>,
}

impl LabelSource {
    pub fn new(fixed: Vec<String>, files: Vec<String>) -> Self {
        Self {
            fixed,
            files,
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// The union of the fixed labels and every readable file, sorted, de-duplicated and capped.
    pub fn current(&self) -> Vec<String> {
        let mut labels = BTreeSet::new();
        for label in &self.fixed {
            self.add(&mut labels, label);
        }
        for file in &self.files {
            if let Some(text) = read_file(Path::new(file)) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    self.add(&mut labels, line);
                }
            }
        }
        labels.into_iter().take(MAX_NODE_LABELS).collect()
    }

    fn add(&self, labels: &mut BTreeSet<String>, label: &str) {
        if valid_label(label) {
            labels.insert(label.to_owned());
            return;
        }
        let key = format!("label:{label}");
        let mut warned = self
            .warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if warned.insert(key) {
            warn!(
                label = %printable(label),
                "ignoring an invalid node label"
            );
        }
    }
}

fn read_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_LABEL_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= MAX_LABEL_FILE_BYTES).then_some(text)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lu-labels-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn files_ignore_comments_and_blanks_and_union_with_fixed_labels() {
        let dir = scratch("union");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::write(&a, "# comment\n\n php8.3 \ngpu\nbad label\nbad label\n").unwrap();
        std::fs::write(&b, "gpu\nregion=eu\n").unwrap();
        let source = LabelSource::new(
            vec!["fixed".to_owned(), "php8.3".to_owned()],
            vec![a.display().to_string(), b.display().to_string()],
        );
        assert_eq!(
            source.current(),
            vec!["fixed", "gpu", "php8.3", "region=eu"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_heartbeat_rereads_the_files() {
        let dir = scratch("reread");
        let file = dir.join("labels");
        std::fs::write(&file, "one\n").unwrap();
        let source = LabelSource::new(Vec::new(), vec![file.display().to_string()]);
        assert_eq!(source.current(), vec!["one"]);
        std::fs::write(&file, "two\n").unwrap();
        assert_eq!(source.current(), vec!["two"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_oversized_files_contribute_nothing() {
        let dir = scratch("missing");
        let big = dir.join("big");
        std::fs::write(&big, "a".repeat(MAX_LABEL_FILE_BYTES as usize + 1)).unwrap();
        let source = LabelSource::new(
            vec!["kept".to_owned()],
            vec![
                dir.join("gone").display().to_string(),
                big.display().to_string(),
            ],
        );
        assert_eq!(source.current(), vec!["kept"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_union_is_capped() {
        let dir = scratch("cap");
        let file = dir.join("labels");
        let text: String = (0..40).map(|i| format!("label-{i:02}\n")).collect();
        std::fs::write(&file, text).unwrap();
        let source = LabelSource::new(Vec::new(), vec![file.display().to_string()]);
        assert_eq!(source.current().len(), MAX_NODE_LABELS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
