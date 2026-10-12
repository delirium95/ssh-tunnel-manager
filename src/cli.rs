use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use glob::Pattern;

use crate::{
    config::Config,
    daemon,
    ipc::{self, Request, Snapshot, Status},
    paths::{self, Paths},
    ssh,
};

#[derive(Debug, Parser)]
#[command(
    name = "boring-rs",
    version,
    about = "An SSH tunnel manager in Rust",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[arg(
        short = 'f',
        long,
        global = true,
        help = "TOML config (default: BORING_CONFIG or Boring's standard path)"
    )]
    pub config: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Private runtime directory; isolates this manager instance"
    )]
    pub runtime_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub daemon: bool,
    #[arg(long, value_enum, help = "Print shell completion setup")]
    pub shell: Option<Shell>,
    #[command(subcommand)]
    pub command: Option<Action>,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    #[command(alias = "o", about = "Open named tunnels in the background")]
    Open {
        #[command(flatten)]
        selection: Selection,
        #[arg(
            long,
            help = "Print SSH arguments without starting a daemon or connecting"
        )]
        dry_run: bool,
    },
    #[command(alias = "c", about = "Close tunnels, even if removed from the config")]
    Close {
        #[command(flatten)]
        selection: Selection,
    },
    #[command(aliases = ["l", "ls"], about = "List configured and running tunnels")]
    List {
        #[arg(short = 'g', long)]
        group: Option<String>,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Create an example config without overwriting an existing file")]
    Init,
    #[command(alias = "e", about = "Edit the config using VISUAL or EDITOR")]
    Edit,
    #[command(about = "Stop the background manager and all its tunnels")]
    Shutdown,
    #[command(alias = "v")]
    Version,
    #[command(alias = "h")]
    Help,
    #[command(about = "Print shell completion setup")]
    Completions { shell: Shell },
    #[command(name = "__complete", hide = true)]
    Complete {
        #[arg(value_parser = ["names", "groups"])]
        kind: String,
        #[arg(long)]
        running: bool,
    },
}

#[derive(Debug, Default, Args)]
pub struct Selection {
    #[arg(short = 'a', long, conflicts_with_all = ["group", "patterns"])]
    pub all: bool,
    #[arg(short = 'g', long, conflicts_with = "patterns")]
    pub group: Option<String>,
    #[arg(help = "Tunnel names or quoted glob patterns, e.g. 'dev-*'")]
    pub patterns: Vec<String>,
}

impl Selection {
    pub fn select(&self, entries: &[Snapshot]) -> Result<Vec<String>> {
        ensure!(
            self.all || self.group.is_some() || !self.patterns.is_empty(),
            "choose tunnel names/patterns, --all, or --group GROUP"
        );
        let group = self
            .group
            .as_deref()
            .map(|group| if group == "default" { "" } else { group });
        let patterns = self
            .patterns
            .iter()
            .map(|pattern| {
                Pattern::new(pattern).with_context(|| format!("invalid glob pattern {pattern:?}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let names: Vec<_> = entries
            .iter()
            .filter(|entry| {
                self.all
                    || group.is_some_and(|group| entry.tunnel.group == group)
                    || patterns
                        .iter()
                        .any(|pattern| pattern.matches(&entry.tunnel.name))
            })
            .map(|entry| entry.tunnel.name.clone())
            .collect();
        ensure!(!names.is_empty(), "no tunnels match this selection");
        for (source, pattern) in self.patterns.iter().zip(patterns.iter()) {
            if !entries
                .iter()
                .any(|entry| pattern.matches(&entry.tunnel.name))
            {
                eprintln!("warning: no tunnels match {source:?}");
            }
        }
        Ok(names)
    }
}

fn configured(config: Config) -> Vec<Snapshot> {
    config
        .tunnels
        .into_iter()
        .map(|tunnel| Snapshot {
            tunnel,
            status: Status::Closed,
            connected_at: None,
            retries: 0,
            last_error: None,
        })
        .collect()
}

fn running(paths: &Paths) -> Result<Vec<Snapshot>> {
    match ipc::call(&paths.socket, &Request::List) {
        Ok(response) => { ensure!(response.errors.is_empty(), "{}", response.errors.join("\n")); Ok(response.tunnels) }
        Err(error) if !paths.socket.exists() => { let _ = error; Ok(Vec::new()) }
        Err(error) => Err(error).context("manager socket exists but is unresponsive; try `boring-rs shutdown` or restart the manager"),
    }
}

fn merged(config: Config, live: Vec<Snapshot>) -> Vec<Snapshot> {
    let mut live: BTreeMap<_, _> = live
        .into_iter()
        .map(|entry| (entry.tunnel.name.clone(), entry))
        .collect();
    let mut entries: Vec<_> = configured(config)
        .into_iter()
        .map(|entry| live.remove(&entry.tunnel.name).unwrap_or(entry))
        .collect();
    entries.extend(live.into_values());
    entries
}

pub fn run(cli: Cli) -> Result<()> {
    if let Some(shell) = cli.shell {
        return completions(shell);
    }
    let paths = Paths::new(cli.runtime_dir)?;
    if cli.daemon {
        return daemon::run(&paths);
    }
    let config_path = match cli.config {
        Some(path) => path,
        None => paths::config_path()?,
    };
    let action = cli.command.unwrap_or(Action::Help);
    match action {
        Action::Open { selection, dry_run } => {
            let entries = configured(Config::load(&config_path)?);
            let names = selection.select(&entries)?;
            let tunnels: Vec<_> = entries
                .into_iter()
                .filter(|entry| names.contains(&entry.tunnel.name))
                .map(|entry| entry.tunnel)
                .collect();
            if dry_run {
                for tunnel in tunnels {
                    let command = ssh::command(&tunnel, &paths.runtime.join("preview.sock"))?;
                    println!("{} [{}]\n{command:?}", tunnel.name, tunnel.mode.label());
                }
                return Ok(());
            }
            daemon::ensure_running(&paths)?;
            let response = ipc::call(&paths.socket, &Request::Open { tunnels })?;
            ensure!(response.errors.is_empty(), "{}", response.errors.join("\n"));
            wait_open(&paths, &names)?;
        }
        Action::Close { selection } => {
            let entries = running(&paths)?;
            let names = selection.select(&entries)?;
            let response = ipc::call(
                &paths.socket,
                &Request::Close {
                    names: names.clone(),
                },
            )?;
            ensure!(response.errors.is_empty(), "{}", response.errors.join("\n"));
            for name in names {
                println!("Closed {name}");
            }
        }
        Action::List { group, json } => {
            let config = if config_path.exists() {
                Config::load(&config_path)?
            } else {
                Config::default()
            };
            let mut entries = merged(config, running(&paths)?);
            if let Some(group) = group {
                entries.retain(|entry| {
                    entry.tunnel.group == if group == "default" { "" } else { &group }
                });
                ensure!(!entries.is_empty(), "no tunnels in group {group:?}");
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                print_table(&entries);
            }
        }
        Action::Init => {
            init_config(&config_path)?;
        }
        Action::Edit => {
            if !config_path.exists() {
                init_config(&config_path)?;
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".into());
            let words = shlex::split(&editor).context("invalid quoting in VISUAL/EDITOR")?;
            let executable = words.first().context("VISUAL/EDITOR is empty")?;
            let status = Command::new(executable)
                .args(&words[1..])
                .arg(&config_path)
                .status()
                .context("cannot start editor")?;
            ensure!(status.success(), "editor exited with {status}");
        }
        Action::Shutdown => {
            if !paths.socket.exists() {
                println!("Manager is not running");
                return Ok(());
            }
            let response = ipc::call(&paths.socket, &Request::Shutdown)?;
            ensure!(response.errors.is_empty(), "{}", response.errors.join("\n"));
            let deadline = Instant::now() + Duration::from_secs(10);
            while paths.socket.exists() {
                ensure!(
                    Instant::now() < deadline,
                    "shutdown is still in progress; see {}",
                    paths.log.display()
                );
                thread::sleep(Duration::from_millis(50));
            }
            println!("Manager stopped; all tunnels closed");
        }
        Action::Version => println!("boring-rs {}", env!("CARGO_PKG_VERSION")),
        Action::Help => {
            Cli::command().print_help()?;
            println!();
        }
        Action::Completions { shell } => completions(shell)?,
        Action::Complete {
            kind,
            running: only_running,
        } => {
            let live = running(&paths).unwrap_or_default();
            let config = Config::load(&config_path).unwrap_or_default();
            let entries = if only_running {
                live
            } else {
                merged(config, live)
            };
            let values: BTreeSet<_> = entries
                .into_iter()
                .map(|entry| {
                    if kind == "groups" {
                        if entry.tunnel.group.is_empty() {
                            "default".into()
                        } else {
                            entry.tunnel.group
                        }
                    } else {
                        entry.tunnel.name
                    }
                })
                .collect();
            for value in values {
                println!("{value}");
            }
        }
    }
    Ok(())
}

fn init_config(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| {
            format!(
                "create {} (existing files are never overwritten)",
                path.display()
            )
        })?;
    file.write_all(include_bytes!("../examples/config.toml"))?;
    println!("Created {}", path.display());
    Ok(())
}

fn wait_open(paths: &Paths, names: &[String]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut pending: BTreeSet<_> = names.iter().cloned().collect();
    let mut errors = Vec::new();
    while !pending.is_empty() {
        let response = ipc::call(&paths.socket, &Request::List)?;
        ensure!(response.errors.is_empty(), "{}", response.errors.join("\n"));
        for entry in response.tunnels {
            if !pending.contains(&entry.tunnel.name) {
                continue;
            }
            match entry.status {
                Status::Open => {
                    println!(
                        "Opened {} [{}] via {}",
                        entry.tunnel.name,
                        entry.tunnel.mode.label(),
                        entry.tunnel.host
                    );
                    pending.remove(&entry.tunnel.name);
                }
                Status::Failed => {
                    pending.remove(&entry.tunnel.name);
                    errors.push(format!(
                        "{}: {}",
                        entry.tunnel.name,
                        entry.last_error.unwrap_or_default()
                    ));
                }
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            // Accepted requests remain visible and can be closed even if CLI waiting expires.
            bail!(
                "still connecting: {}; inspect `list --json` or close them",
                pending.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        if !pending.is_empty() {
            thread::sleep(Duration::from_millis(100));
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

fn print_table(entries: &[Snapshot]) {
    if entries.is_empty() {
        println!("No tunnels configured");
        return;
    }
    println!(
        "{:<14} {:<22} {:<14} {:<16} {:<24} {:<24} VIA",
        "STATUS", "NAME", "GROUP", "MODE", "LOCAL", "REMOTE"
    );
    for entry in entries {
        println!(
            "{:<14} {:<22} {:<14} {:<16} {:<24} {:<24} {}",
            entry.status.label(),
            entry.tunnel.name,
            if entry.tunnel.group.is_empty() {
                "default"
            } else {
                &entry.tunnel.group
            },
            entry.tunnel.mode.label(),
            if entry.tunnel.local.is_empty() {
                "[SOCKS]"
            } else {
                &entry.tunnel.local
            },
            if entry.tunnel.remote.is_empty() {
                "[SOCKS]"
            } else {
                &entry.tunnel.remote
            },
            entry.tunnel.host
        );
        if let Some(error) = &entry.last_error {
            println!("  {}", error.replace('\n', " | "));
        }
    }
}

fn completions(shell: Shell) -> Result<()> {
    let text = match shell {
        Shell::Bash => include_str!("../completions/boring-rs.bash"),
        Shell::Zsh => include_str!("../completions/boring-rs.zsh"),
        Shell::Fish => include_str!("../completions/boring-rs.fish"),
        _ => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "boring-rs",
                &mut std::io::stdout(),
            );
            return Ok(());
        }
    };
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_supports_union_groups_and_rejects_bad_globs() {
        let entries = configured(Config::parse("[[tunnels]]\nname='dev-db'\nhost='server'\nlocal=9000\nremote='localhost:80'\ngroup='dev'\n[[tunnels]]\nname='prod'\nhost='server'\nmode='socks'\nlocal=1080").unwrap());
        assert_eq!(
            Selection {
                patterns: vec!["dev-*".into(), "prod".into()],
                ..Selection::default()
            }
            .select(&entries)
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            Selection {
                group: Some("default".into()),
                ..Selection::default()
            }
            .select(&entries)
            .unwrap(),
            ["prod"]
        );
        assert!(
            Selection {
                patterns: vec!["[".into()],
                ..Selection::default()
            }
            .select(&entries)
            .is_err()
        );
        assert!(Selection::default().select(&entries).is_err());
        Cli::command().debug_assert();
    }
}
