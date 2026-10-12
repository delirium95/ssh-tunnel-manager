use std::{
    env,
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

#[derive(Clone, Debug)]
pub struct Paths {
    pub runtime: PathBuf,
    pub socket: PathBuf,
    pub log: PathBuf,
}

pub fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn expand_home(path: &str) -> Result<String> {
    if path == "~" {
        return Ok(home()?.to_string_lossy().into_owned());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home()?.join(rest).to_string_lossy().into_owned());
    }
    Ok(path.to_owned())
}

pub fn config_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("BORING_CONFIG") {
        return Ok(PathBuf::from(expand_home(&path)?));
    }
    if cfg!(target_os = "linux") {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".config"));
        Ok(base.join("boring/.boring.toml"))
    } else {
        Ok(home()?.join(".boring.toml"))
    }
}

impl Paths {
    pub fn new(runtime: Option<PathBuf>) -> Result<Self> {
        // A short, user-owned directory also avoids Unix socket path length limits.
        let uid = unsafe { libc::geteuid() };
        let runtime = runtime
            .or_else(|| env::var_os("BORING_RUNTIME_DIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/boring-rs-{uid}")));
        ensure!(runtime.is_absolute(), "runtime directory must be absolute");
        let socket = env::var_os("BORING_SOCK")
            .map(PathBuf::from)
            .unwrap_or(runtime.join("daemon.sock"));
        let log = env::var_os("BORING_LOG_FILE")
            .map(PathBuf::from)
            .unwrap_or(runtime.join("daemon.log"));
        ensure!(
            socket.is_absolute() && socket.as_os_str().len() < 100,
            "IPC socket path must be absolute and shorter than 100 bytes"
        );
        ensure!(
            runtime.as_os_str().len() < 75,
            "runtime path must be shorter than 75 bytes (SSH control socket limit)"
        );
        Ok(Self {
            runtime,
            socket,
            log,
        })
    }

    pub fn prepare(&self) -> Result<()> {
        private_directory(&self.runtime)?;
        if let Some(parent) = self.socket.parent() {
            private_directory(parent)?;
        }
        Ok(())
    }
}

pub fn private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "create private directory {} (parent must exist)",
                        path.display()
                    )
                });
            }
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{} must be a real directory",
        path.display()
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "{} is owned by another user",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "{} must have mode 0700",
        path.display()
    );
    Ok(())
}

pub fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.permissions().mode() & 0o077 == 0,
        "{} must be a private regular file owned by the current user",
        path.display()
    );
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_insecure_runtime_and_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(directory.path()).is_err());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        private_directory(directory.path()).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(directory.path().join("missing"), &link).unwrap();
        assert!(private_file(&link).is_err());
    }
}
