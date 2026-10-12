use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};

use crate::{
    config::{DEFAULT_KEEP_ALIVE, Mode, Tunnel},
    paths,
};

pub fn command(tunnel: &Tunnel, control: &Path) -> Result<Command> {
    let (flag, forwarding) = tunnel.forwarding()?;
    let mut command = Command::new("ssh");
    command.args(["-N", "-T", "-n", "-M", "-S"]).arg(control);
    for option in [
        "BatchMode=yes".to_owned(),
        "StrictHostKeyChecking=yes".into(),
        "ExitOnForwardFailure=yes".into(),
        "ConnectTimeout=10".into(),
        "ConnectionAttempts=1".into(),
        format!(
            "ServerAliveInterval={}",
            tunnel.keep_alive.unwrap_or(DEFAULT_KEEP_ALIVE)
        ),
        "ServerAliveCountMax=3".into(),
        "ControlPersist=no".into(),
        "ForkAfterAuthentication=no".into(),
        "PermitLocalCommand=no".into(),
        "StdinNull=yes".into(),
    ] {
        command.args(["-o", &option]);
    }
    if !tunnel.user.is_empty() {
        command.arg("-l").arg(&tunnel.user);
    }
    if !tunnel.port.is_empty() {
        command.arg("-p").arg(&tunnel.port);
    }
    if !tunnel.identity.is_empty() {
        command.arg("-i").arg(&tunnel.identity);
    }
    command.arg(flag).arg(forwarding).arg(&tunnel.host);
    Ok(command)
}

pub struct Session {
    pub child: Child,
    pub control: PathBuf,
    pub log: PathBuf,
    stopped: bool,
    local_socket: Option<PathBuf>,
    socket_inode: Option<(u64, u64)>,
}

impl Session {
    pub fn start(tunnel: &Tunnel, control: PathBuf, log: PathBuf) -> Result<Self> {
        let local_socket = (tunnel.mode == Mode::Local && tunnel.local.starts_with('/'))
            .then(|| PathBuf::from(&tunnel.local));
        if let Some(path) = &local_socket {
            ensure!(
                fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "local Unix socket already exists: {}; remove it only after checking its owner",
                path.display()
            );
        }
        let file = paths::private_file(&log)?;
        file.set_len(0)?;
        let mut command = command(tunnel, &control)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(file)
            .process_group(0);
        // On Linux, terminate SSH if the manager is killed unexpectedly.
        // Normal shutdown on every supported platform kills and reaps the whole group.
        #[cfg(target_os = "linux")]
        {
            let parent = unsafe { libc::getpid() };
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        libc::_exit(1);
                    }
                    Ok(())
                });
            }
        }
        let child = command
            .spawn()
            .context("cannot start OpenSSH; install ssh and put it in PATH")?;
        Ok(Self {
            child,
            control,
            log,
            stopped: false,
            local_socket,
            socket_inode: None,
        })
    }

    pub fn observe_listener(&mut self) {
        if self.socket_inode.is_none()
            && let Some(path) = &self.local_socket
            && let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
            && metadata.uid() == unsafe { libc::geteuid() }
        {
            self.socket_inode = Some((metadata.dev(), metadata.ino()));
        }
    }

    fn cleanup_listener(&self) {
        if let (Some(path), Some(inode)) = (&self.local_socket, self.socket_inode)
            && let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == inode
        {
            let _ = fs::remove_file(path);
        }
    }

    pub fn ready(&self, host: &str) -> bool {
        if !self.control.exists() {
            return false;
        }
        let mut command = Command::new("ssh");
        command
            .arg("-S")
            .arg(&self.control)
            .args(["-O", "check", host]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    pub fn error_tail(&self) -> String {
        let Ok(mut file) = fs::File::open(&self.log) else {
            return "SSH exited without a diagnostic".into();
        };
        let length = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(length.saturating_sub(4096)));
        let mut bytes = Vec::new();
        let _ = file.take(4096).read_to_end(&mut bytes);
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if text.is_empty() {
            "SSH connection ended".into()
        } else {
            text
        }
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.observe_listener();
        // Never signal a PID after it has been reaped (it could have been reused).
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = fs::remove_file(&self.control);
            self.cleanup_listener();
            return;
        }
        // Process groups also include ProxyJump/ProxyCommand children.
        let pid = self.child.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(50);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    let _ = fs::remove_file(&self.control);
                    self.cleanup_listener();
                    return;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    break;
                }
            }
        }
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.control);
        self.cleanup_listener();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn command_preserves_arguments_and_strict_host_keys() {
        let config = Config::parse("[[tunnels]]\nname='dev'\nhost='server'\nlocal=9000\nremote='localhost:80'\nidentity='/tmp/key with spaces'").unwrap();
        let command = command(&config.tunnels[0], Path::new("/tmp/control")).unwrap();
        let args: Vec<_> = command.get_args().map(|s| s.to_string_lossy()).collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-i", "/tmp/key with spaces"])
        );
        assert!(args.iter().any(|arg| arg == "StrictHostKeyChecking=yes"));
        assert_eq!(args.last().unwrap(), "server");
    }
}
