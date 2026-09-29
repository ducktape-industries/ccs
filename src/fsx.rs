//! Filesystem helpers shared by the credential store and the account stash.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `bytes` to `path` atomically, leaving `mode` on the result.
///
/// Staged in a sibling temp file and renamed, so a concurrent reader sees
/// either the old contents or the new and never a partial write. The rename
/// also freshens the path's mtime, which is the signal running Claude Code
/// sessions poll to reload credentials.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let name = path.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{}.ccs-{}.tmp", name.trim_start_matches('.'), std::process::id()));

    let staged = || -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    };
    if let Err(e) = staged() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp, path) {
        // The rename is what makes the staged file the real one; failing to
        // land it is exactly as much a failure as failing to write it, and
        // leaving the stage behind would leak whatever `bytes` held under a
        // name every pen's mirror otherwise has to know to skip.
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("installing {}", path.display()));
    }
    // Durability of the rename itself. Best-effort: it has already taken effect
    // for every reader by this point.
    let _ = File::open(dir).and_then(|d| d.sync_all());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A private root per test, so concurrently running tests never share a path.
    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ccs-fsx-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_failed_rename_does_not_leave_the_staged_file_behind() {
        let dir = temp("rename-fails");
        // A directory already sits where the write wants to land; renaming a
        // regular file onto an existing directory fails.
        let path = dir.join("target");
        fs::create_dir(&path).expect("occupy the target");

        assert!(write_atomic(&path, b"data", 0o600).is_err());

        let staged = fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".ccs-"));
        assert!(!staged, "a failed rename must not leave its staged file behind");
    }

    #[test]
    fn a_successful_write_lands_at_the_path_with_the_given_mode() {
        let dir = temp("success");
        let path = dir.join("target");

        write_atomic(&path, b"data", 0o600).expect("write");

        assert_eq!(fs::read(&path).expect("read"), b"data");
        assert_eq!(fs::metadata(&path).expect("meta").permissions().mode() & 0o777, 0o600);
        assert!(
            fs::read_dir(&dir).expect("read dir").flatten().count() == 1,
            "no staged file should remain beside a successful write"
        );
    }
}
