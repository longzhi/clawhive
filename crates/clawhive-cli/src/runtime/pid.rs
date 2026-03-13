use std::path::{Path, PathBuf};

use anyhow::Result;

pub(crate) fn pid_file_path(root: &Path) -> PathBuf {
    root.join("clawhive.pid")
}

pub(crate) fn port_file_path(root: &Path) -> PathBuf {
    root.join("clawhive.port")
}

pub(crate) fn write_pid_file(root: &Path) -> Result<()> {
    let path = pid_file_path(root);
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

pub(crate) fn read_pid_file(root: &Path) -> Result<Option<u32>> {
    let path = pid_file_path(root);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let pid = content.trim().parse::<u32>()?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn remove_pid_file(root: &Path) {
    let _ = std::fs::remove_file(pid_file_path(root));
}

pub(crate) fn write_port_file(root: &Path, port: u16) -> Result<()> {
    std::fs::write(port_file_path(root), port.to_string())?;
    Ok(())
}

pub(crate) fn read_port_file(root: &Path) -> Result<Option<u16>> {
    let path = port_file_path(root);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let port = content.trim().parse::<u16>()?;
            Ok(Some(port))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn remove_port_file(root: &Path) {
    let _ = std::fs::remove_file(port_file_path(root));
}

pub(crate) fn is_process_running(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) sends no signal; it only checks whether the process
    // exists. `pid as i32` is always a valid i32 since pid_t is i32 on
    // Linux/macOS. No memory is read or written.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Check for stale PID file. Returns error if another instance is running.
pub(crate) fn check_and_clean_pid(root: &Path) -> Result<()> {
    if let Some(pid) = read_pid_file(root)? {
        if is_process_running(pid) {
            anyhow::bail!("clawhive is already running (pid: {pid}). Use 'clawhive stop' first.");
        }
        tracing::info!("Removing stale PID file (pid: {pid}, process not running)");
        remove_pid_file(root);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_write_read_remove() {
        let tmp = tempfile::tempdir().unwrap();
        write_pid_file(tmp.path()).unwrap();
        let pid = read_pid_file(tmp.path()).unwrap();
        assert_eq!(pid, Some(std::process::id()));
        remove_pid_file(tmp.path());
        let pid = read_pid_file(tmp.path()).unwrap();
        assert_eq!(pid, None);
    }

    #[test]
    fn port_file_write_read_remove() {
        let tmp = tempfile::tempdir().unwrap();
        write_port_file(tmp.path(), 8848).unwrap();
        let port = read_port_file(tmp.path()).unwrap();
        assert_eq!(port, Some(8848));
        remove_port_file(tmp.path());
        let port = read_port_file(tmp.path()).unwrap();
        assert_eq!(port, None);
    }

    #[test]
    fn read_pid_file_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_pid_file(tmp.path()).unwrap(), None);
    }

    #[test]
    fn is_process_running_self() {
        assert!(is_process_running(std::process::id()));
    }

    #[test]
    fn is_process_running_nonexistent() {
        // PID 99999999 almost certainly does not exist
        assert!(!is_process_running(99_999_999));
    }

    #[test]
    fn check_and_clean_pid_stale() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a fake PID that doesn't exist
        std::fs::write(tmp.path().join("clawhive.pid"), "99999999").unwrap();
        // Should clean up the stale PID file
        check_and_clean_pid(tmp.path()).unwrap();
        assert_eq!(read_pid_file(tmp.path()).unwrap(), None);
    }

    #[test]
    fn check_and_clean_pid_active_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("clawhive.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        let result = check_and_clean_pid(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already running"));
    }
}
