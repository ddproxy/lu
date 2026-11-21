#![no_main]
#![forbid(unsafe_code)]

use std::fs;
use std::path::{Component, PathBuf};
use std::str::FromStr;

use axum::body::to_bytes;
use libfuzzer_sys::fuzz_target;
use lu_controller::routes::file_body::{file_len, stream_range};
use lu_proto::{ArtifactName, JobId};
use lu_staging::{ArtifactDir, ArtifactFile};

const SECRET: &[u8] = b"lu-fuzz-secret";
const VALID_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lu-fuzz-artifact-path-{}-{}",
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

fn looks_like_job_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

// Artifact and log paths are derived from agent-supplied names. Accepted paths must stay below
// the artifact root, downloads must refuse a symlink to a secret, and a write must not follow it.
fuzz_target!(|data: &[u8]| {
    let candidate = String::from_utf8_lossy(data);
    let root = TempDir::new();
    let artifacts = root.0.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifact root");
    let artifact_dir = ArtifactDir::new(&artifacts).expect("accept real artifact root");

    if let Ok(name) = ArtifactName::from_str(candidate.as_ref()) {
        let file_name = name.file_name();
        assert!(!file_name.contains('/') && !file_name.contains('\\'));
        let job_id = JobId::parse(VALID_ID).expect("valid fixture id");
        assert_eq!(
            name.relative_path(&job_id),
            format!("{VALID_ID}/{file_name}"),
            "artifact path must be job id plus a fixed file name"
        );
        assert!(name.max_bytes() > 0);
    }

    let cap = data.len() as u64;
    let created = artifact_dir.create(candidate.as_ref());
    if let Ok(path) = &created {
        let relative = path.strip_prefix(&artifacts).expect("path below root");
        assert!(
            relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "job id escaped the artifact root: {}",
            relative.display()
        );
    }

    if !looks_like_job_id(candidate.as_ref()) {
        return;
    }

    let dir = match created {
        Ok(path) => path,
        Err(_) => {
            let path = artifacts.join(candidate.as_ref());
            fs::create_dir_all(&path).expect("create job artifact dir");
            path
        }
    };
    let log = dir.join("run.log");
    let secret = root.0.join("secret");
    fs::write(&secret, SECRET).expect("write secret fixture");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build fuzz runtime");
    rt.block_on(async {
        #[cfg(unix)]
        if std::os::unix::fs::symlink(&secret, &log).is_ok() {
            let len = file_len(&log, cap).await;
            if let Err(error) = len {
                assert!(
                    !error.message.contains(std::str::from_utf8(SECRET).unwrap()),
                    "an error leaked the secret"
                );
            }
            match stream_range(&log, cap, 0, cap).await {
                Ok(file) => {
                    let bytes = to_bytes(file.body, cap.saturating_add(1) as usize)
                        .await
                        .expect("read streamed log");
                    assert!(
                        !bytes.windows(SECRET.len()).any(|w| w == SECRET),
                        "a symlinked secret was streamed"
                    );
                }
                Err(error) => assert!(
                    !error.message.contains(std::str::from_utf8(SECRET).unwrap()),
                    "an error leaked the secret"
                ),
            }
        }

        let written = artifact_dir.write(candidate.as_ref(), ArtifactFile::RunLog, data, cap);
        match written {
            Ok(bytes) => {
                assert!(
                    bytes <= data.len() as u64,
                    "write returned more than it read"
                );
                let stored = fs::read(&log).expect("read stored artifact");
                assert!(stored.len() <= data.len(), "stored artifact grew");
                assert!(stored == data[..stored.len()], "stored bytes were altered");
            }
            Err(error) => assert!(
                error.to_string().len() <= data.len().saturating_add(256),
                "unbounded artifact error"
            ),
        }
    });
});
