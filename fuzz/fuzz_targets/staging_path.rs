#![no_main]
#![forbid(unsafe_code)]

use std::fs;
use std::path::{Component, Path, PathBuf};

use libfuzzer_sys::fuzz_target;
use lu_staging::{create_input_dir, is_lu_path, safe_input_name, write_input_file};

const SECRET: &[u8] = b"lu-fuzz-secret";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lu-fuzz-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create fuzz temp directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Path text and filesystem state are both attacker-controlled here: nested `.lu` components,
// traversal, symlinks and hostile input names. Accepted names must stay simple, and no error
// message may echo the secret behind a symlink.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let path = Path::new(text.as_ref());

    let expected_lu = text.split('/').any(|part| part == ".lu");
    assert_eq!(
        is_lu_path(path),
        expected_lu,
        "nested .lu detection disagreed with the literal path: {text:?}"
    );

    let name = safe_input_name(Some(&text), "job-fallback");
    assert!(!name.is_empty(), "a safe input name must not be empty");
    assert!(name.len() <= 128, "a safe input name must be capped");
    assert!(
        Path::new(&name)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "a safe input name must have no traversal component: {name:?}"
    );
    assert!(
        !name.starts_with('.'),
        "a safe input name must not hide: {name:?}"
    );

    let root = TempDir::new("staging-path");
    let inputs = root.0.join("inputs");
    fs::create_dir_all(&inputs).expect("create inputs directory");

    let secret = root.0.join("secret");
    fs::write(&secret, SECRET).expect("write secret fixture");

    #[cfg(unix)]
    {
        let link = inputs.join(&name);
        if std::os::unix::fs::symlink(&secret, &link).is_ok() {
            let error = create_input_dir(&inputs, &name).unwrap_err().to_string();
            assert!(
                !error.contains(std::str::from_utf8(SECRET).unwrap()),
                "{error}"
            );
            assert!(error.len() <= text.len().saturating_add(256), "{error}");
        }
    }

    match create_input_dir(&inputs, &name) {
        Ok(dir) => {
            let canonical_inputs = inputs.canonicalize().expect("canonical inputs");
            let canonical_dir = dir.canonicalize().expect("canonical input dir");
            assert!(
                canonical_dir.starts_with(&canonical_inputs),
                "input dir escaped its parent: {}",
                canonical_dir.display()
            );
            let written = write_input_file(&dir, "diff.patch", data);
            if written.is_ok() {
                let bytes = fs::read(dir.join("diff.patch")).expect("read patched input");
                assert!(
                    bytes.len() <= data.len(),
                    "input file grew beyond its source"
                );
            }
            if let Err(error) = written {
                let message = error.to_string();
                assert!(
                    message.len() <= text.len().saturating_add(256),
                    "unbounded write error"
                );
            }
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.len() <= text.len().saturating_add(256),
                "unbounded input dir error"
            );
        }
    }
});
