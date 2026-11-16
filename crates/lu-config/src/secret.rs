use std::io::Read;
use std::path::Path;

use crate::ConfigError;

/// Reads a secret file after opening it once and validating it on the open handle.
/// Refuses symlinks/FIFOs/non-regular files, wrong owner, group/other bits, and oversize files.
pub fn read_private_file(what: &str, path: &Path, max_bytes: u64) -> Result<String, ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|source| ConfigError::Io {
                path: path.display().to_string(),
                source,
            })?;
        let meta = file.metadata().map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let ft = meta.file_type();
        if !ft.is_file() {
            return Err(ConfigError::Invalid {
                table: what.to_owned(),
                reason: format!("{} must be a regular file", path.display()),
            });
        }
        let euid = rustix::process::geteuid().as_raw();
        if meta.uid() != euid {
            return Err(ConfigError::Invalid {
                table: what.to_owned(),
                reason: format!("{} must be owned by the effective user", path.display()),
            });
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ConfigError::InsecureFile {
                what: what.to_owned(),
                path: path.display().to_string(),
                mode,
            });
        }
        let size = meta.len();
        if size > max_bytes {
            return Err(ConfigError::TooLarge {
                path: path.display().to_string(),
                limit: max_bytes,
            });
        }
        let mut text = String::new();
        file.take(max_bytes + 1)
            .read_to_string(&mut text)
            .map_err(|source| ConfigError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if text.len() as u64 > max_bytes {
            return Err(ConfigError::TooLarge {
                path: path.display().to_string(),
                limit: max_bytes,
            });
        }
        Ok(text)
    }
    #[cfg(not(unix))]
    {
        let _ = (what, path, max_bytes);
        Err(ConfigError::Invalid {
            table: what.to_owned(),
            reason: "secret file reading is only supported on unix".to_owned(),
        })
    }
}

/// Compatibility wrapper for callers that only need the validation check.
pub fn check_private_file(what: &str, path: &Path) -> Result<(), ConfigError> {
    // Large limit: check-only callers do not need content, but the path must be opened once.
    read_private_file(what, path, u64::MAX / 2).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lu-secret-{name}-{}", std::process::id()))
    }

    #[test]
    fn symlink_refused() {
        let dir = tmp("symlink");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good");
        std::fs::write(&good, "ok").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link");
        symlink(&good, &link).unwrap();
        assert!(read_private_file("[t]", &link, 1024).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fifo_refused_without_hanging() {
        let dir = tmp("fifo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("f");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        // If open blocks, this test would hang; O_NONBLOCK prevents that.
        assert!(read_private_file("[t]", &fifo, 1024).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_refused() {
        let dir = tmp("dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_private_file("[t]", &dir, 1024).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_bits_enforced() {
        let dir = tmp("modes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (mode, ok) in [(0o640, false), (0o604, false), (0o600, true), (0o400, true)] {
            let p = dir.join(format!("{mode:o}"));
            std::fs::write(&p, "secret").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            let res = read_private_file("[t]", &p, 1024);
            if ok {
                assert_eq!(res.unwrap(), "secret", "mode {mode:o} should pass");
            } else {
                assert!(res.is_err(), "mode {mode:o} should be refused");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversize_refused() {
        let dir = tmp("size");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big");
        std::fs::write(&p, "0123456789").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_private_file("[t]", &p, 5).is_err());
        assert!(read_private_file("[t]", &p, 10).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
