use std::{
    collections::BTreeMap,
    fs,
    io::{ErrorKind, Write},
    os::unix::{fs::FileTypeExt, net::UnixListener, process::CommandExt},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;

use crate::{
    config::{DEFAULT_RECONNECT_TIMEOUT, Tunnel},
    ipc::{self, Request, Response, Snapshot, Status},
    paths::{self, Paths},
    ssh::Session,
};

const TICK: Duration = Duration::from_millis(50);
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub fn ensure_running(paths: &Paths) -> Result<()> {
    paths.prepare()?;
    if ipc::call(&paths.socket, &Request::Ping).is_ok() {
        return Ok(());
    }
    let log = paths::private_file(&paths.log)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--runtime-dir")
        .arg(&paths.runtime)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .current_dir("/");
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("cannot start background manager")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ipc::call(&paths.socket, &Request::Ping).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            // Another simultaneous caller may have won the daemon lock.
            thread::sleep(TICK);
            if ipc::call(&paths.socket, &Request::Ping).is_ok() {
                return Ok(());
            }
            bail!("daemon exited with {status}; see {}", paths.log.display());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("daemon startup timed out; see {}", paths.log.display());
        }
        thread::sleep(TICK);
    }
}

struct Worker {
    snapshot: Snapshot,
    session: Option<Session>,
    control: std::path::PathBuf,
    log: std::path::PathBuf,
    attempt_started: Instant,
    reconnect_started: Option<Instant>,
    next_attempt: Instant,
    next_check: Instant,
    backoff: Duration,
    has_connected: bool,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.session.take();
        let _ = fs::remove_file(&self.log);
    }
}

impl Worker {
    fn new(tunnel: Tunnel, paths: &Paths, id: u64) -> Result<Self> {
        tunnel.validate()?;
        let now = Instant::now();
        let stem = format!("t{}-{id}", std::process::id());
        let control = paths.runtime.join(format!("{stem}.sock"));
        let log = paths.runtime.join(format!("{stem}.log"));
        let session = Session::start(&tunnel, control.clone(), log.clone())?;
        Ok(Self {
            snapshot: Snapshot {
                tunnel,
                status: Status::Connecting,
                connected_at: None,
                retries: 0,
                last_error: None,
            },
            session: Some(session),
            control,
            log,
            attempt_started: now,
            reconnect_started: None,
            next_attempt: now,
            next_check: now,
            backoff: INITIAL_BACKOFF,
            has_connected: false,
        })
    }

    fn failure(&mut self, error: String) {
        self.session.take();
        self.snapshot.last_error = Some(error);
        self.snapshot.connected_at = None;
        let timeout = self
            .snapshot
            .tunnel
            .reconnect_timeout
            .unwrap_or(DEFAULT_RECONNECT_TIMEOUT);
        if !self.has_connected || timeout == 0 {
            self.snapshot.status = Status::Failed;
            return;
        }
        let now = Instant::now();
        self.reconnect_started.get_or_insert(now);
        self.snapshot.status = Status::Reconnecting;
        self.next_attempt = now + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(start) = self.reconnect_started
            && now.duration_since(start).as_secs()
                >= self
                    .snapshot
                    .tunnel
                    .reconnect_timeout
                    .unwrap_or(DEFAULT_RECONNECT_TIMEOUT)
        {
            self.session.take();
            self.snapshot.status = Status::Failed;
            self.snapshot.last_error = Some(format!(
                "reconnect timeout; {}",
                self.snapshot
                    .last_error
                    .as_deref()
                    .unwrap_or("connection lost")
            ));
            self.reconnect_started = None;
            return;
        }
        if let Some(session) = &mut self.session {
            session.observe_listener();
            match session.child.try_wait() {
                Ok(Some(status)) => {
                    let error = format!("{status}: {}", session.error_tail());
                    self.failure(error);
                    return;
                }
                Err(error) => {
                    self.failure(error.to_string());
                    return;
                }
                Ok(None) => {}
            }
            if self.snapshot.status != Status::Open && now >= self.next_check {
                self.next_check = now + Duration::from_millis(200);
                if session.ready(&self.snapshot.tunnel.host) {
                    self.snapshot.status = Status::Open;
                    self.snapshot.connected_at = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    );
                    self.snapshot.last_error = None;
                    self.has_connected = true;
                    self.reconnect_started = None;
                    self.backoff = INITIAL_BACKOFF;
                } else if now.duration_since(self.attempt_started) >= CONNECT_TIMEOUT {
                    let error = format!("SSH startup timed out: {}", session.error_tail());
                    self.failure(error);
                }
            }
        } else if self.snapshot.status == Status::Reconnecting && now >= self.next_attempt {
            self.snapshot.retries = self.snapshot.retries.saturating_add(1);
            match Session::start(
                &self.snapshot.tunnel,
                self.control.clone(),
                self.log.clone(),
            ) {
                Ok(session) => {
                    self.session = Some(session);
                    self.attempt_started = now;
                    self.next_check = now;
                }
                Err(error) => self.failure(format!("{error:#}")),
            }
        }
    }
}

struct Manager {
    workers: BTreeMap<String, Worker>,
    next_id: u64,
}

impl Manager {
    fn handle(&mut self, request: Request, paths: &Paths, stopping: &AtomicBool) -> Response {
        let mut response = Response {
            pid: std::process::id(),
            ..Response::default()
        };
        match request {
            Request::Ping | Request::List => {}
            Request::Open { tunnels } => {
                for tunnel in tunnels {
                    let name = tunnel.name.clone();
                    if !self.workers.contains_key(&name)
                        && self.workers.len() >= crate::config::MAX_TUNNELS
                    {
                        response
                            .errors
                            .push(format!("{name}: manager tunnel limit reached"));
                        continue;
                    }
                    if let Some(worker) = self.workers.get(&name)
                        && !matches!(worker.snapshot.status, Status::Failed | Status::Closed)
                    {
                        if worker.snapshot.tunnel != tunnel {
                            response.errors.push(format!("{name}: already running with different settings; close it before reopening"));
                        }
                        continue;
                    }
                    self.workers.remove(&name);
                    self.next_id += 1;
                    match Worker::new(tunnel, paths, self.next_id) {
                        Ok(worker) => {
                            self.workers.insert(name, worker);
                        }
                        Err(error) => response.errors.push(format!("{name}: {error:#}")),
                    }
                }
            }
            Request::Close { names } => {
                for name in names {
                    self.workers.remove(&name);
                }
            }
            Request::Shutdown => {
                stopping.store(true, Ordering::Relaxed);
            }
        }
        response.tunnels = self
            .workers
            .values()
            .map(|worker| worker.snapshot.clone())
            .collect();
        response
    }
}

pub fn run(paths: &Paths) -> Result<()> {
    paths.prepare()?;
    let lock = paths::private_file(&paths.runtime.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .context("another manager owns this runtime directory")?;
    if let Ok(metadata) = fs::symlink_metadata(&paths.socket) {
        ensure!(
            metadata.file_type().is_socket(),
            "refusing to remove a non-socket IPC path"
        );
        ensure!(
            ipc::call(&paths.socket, &Request::Ping).is_err(),
            "another manager is already listening at this socket"
        );
        fs::remove_file(&paths.socket)?;
    }
    let listener = UnixListener::bind(&paths.socket)?;
    // The directory is private; restrict the socket itself too.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))?;
    }
    let mut manager = Manager {
        workers: BTreeMap::new(),
        next_id: 0,
    };
    let mut log = paths::private_file(&paths.log)?;
    writeln!(log, "manager {} started", std::process::id())?;
    while !stopping.load(Ordering::Relaxed) {
        for worker in manager.workers.values_mut() {
            worker.tick();
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                let response = match ipc::read(&mut stream) {
                    Ok(request) => manager.handle(request, paths, &stopping),
                    Err(error) => Response {
                        pid: std::process::id(),
                        errors: vec![format!("{error:#}")],
                        ..Response::default()
                    },
                };
                let _ = ipc::write(&response, &mut stream);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(error) => return Err(error).context("accept daemon command"),
        }
    }
    manager.workers.clear();
    drop(listener);
    fs::remove_file(&paths.socket)?;
    writeln!(log, "manager {} stopped", std::process::id())?;
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        let mut delay = INITIAL_BACKOFF;
        for _ in 0..30 {
            delay = (delay * 2).min(MAX_BACKOFF);
        }
        assert_eq!(delay, Duration::from_secs(60));
    }
}
