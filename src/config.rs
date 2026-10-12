use std::{collections::HashSet, env, fs, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize};

pub const DEFAULT_KEEP_ALIVE: u64 = 120;
pub const DEFAULT_RECONNECT_TIMEOUT: u64 = 900;
pub const MAX_TUNNELS: usize = 128;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Mode {
    #[default]
    #[serde(rename = "local", alias = "l", alias = "-l")]
    Local,
    #[serde(rename = "remote", alias = "r", alias = "-r")]
    Remote,
    #[serde(rename = "socks")]
    Socks,
    #[serde(rename = "socks-remote")]
    SocksRemote,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Socks => "socks",
            Self::SocksRemote => "socks-remote",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tunnel {
    pub name: String,
    pub host: String,
    #[serde(default, deserialize_with = "string_or_int")]
    pub local: String,
    #[serde(default, deserialize_with = "string_or_int")]
    pub remote: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub identity: String,
    #[serde(default, deserialize_with = "string_or_int")]
    pub port: String,
    #[serde(default)]
    pub group: String,
    pub keep_alive: Option<u64>,
    pub reconnect_timeout: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub tunnels: Vec<Tunnel>,
    pub keep_alive: Option<u64>,
    pub reconnect_timeout: Option<u64>,
}

fn string_or_int<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Number(i64),
    }
    Ok(match Value::deserialize(deserializer)? {
        Value::Text(text) => text,
        Value::Number(number) => number.to_string(),
    })
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read config {} (try `boring-rs init`)",
                path.display()
            )
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text).context("invalid TOML configuration")?;
        ensure!(
            config.tunnels.len() <= MAX_TUNNELS,
            "at most {MAX_TUNNELS} tunnels are supported per manager"
        );
        let mut names = HashSet::new();
        for tunnel in &mut config.tunnels {
            ensure!(
                names.insert(tunnel.name.clone()),
                "duplicate tunnel {:?}",
                tunnel.name
            );
            for field in [
                &mut tunnel.host,
                &mut tunnel.user,
                &mut tunnel.identity,
                &mut tunnel.port,
                &mut tunnel.local,
                &mut tunnel.remote,
            ] {
                *field = expand_env(field, |name| env::var(name).ok())?;
            }
            tunnel.identity = crate::paths::expand_home(&tunnel.identity)?;
            for address in [&mut tunnel.local, &mut tunnel.remote] {
                if address.starts_with("~/") {
                    *address = crate::paths::expand_home(address)?;
                }
            }
            tunnel.keep_alive = Some(
                tunnel
                    .keep_alive
                    .or(config.keep_alive)
                    .unwrap_or(DEFAULT_KEEP_ALIVE),
            );
            tunnel.reconnect_timeout = Some(
                tunnel
                    .reconnect_timeout
                    .or(config.reconnect_timeout)
                    .unwrap_or(DEFAULT_RECONNECT_TIMEOUT),
            );
            tunnel
                .validate()
                .with_context(|| format!("tunnel {:?}", tunnel.name))?;
        }
        Ok(config)
    }
}

impl Tunnel {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if !self.group.is_empty() {
            validate_name(&self.group)?;
            ensure!(
                self.group != "default",
                "group 'default' is reserved for ungrouped tunnels"
            );
        }
        ensure!(
            valid_host(&self.host),
            "invalid SSH host {:?}; use --user/config user instead of user@host",
            self.host
        );
        ensure!(
            self.user.is_empty() || valid_token(&self.user),
            "invalid SSH user"
        );
        ensure!(
            self.port.is_empty() || valid_port(&self.port),
            "SSH port must be 1..65535"
        );
        ensure!(
            !self.identity.contains(['\0', '\n', '\r']),
            "invalid identity path"
        );
        ensure!(
            self.keep_alive.unwrap_or(DEFAULT_KEEP_ALIVE) <= 86400,
            "keep_alive must be 0..86400 seconds"
        );
        ensure!(
            self.reconnect_timeout.unwrap_or(DEFAULT_RECONNECT_TIMEOUT) <= 86400,
            "reconnect_timeout must be 0..86400 seconds"
        );
        match self.mode {
            Mode::Local | Mode::Remote => {
                address(&self.local, self.mode == Mode::Local)?;
                address(&self.remote, self.mode == Mode::Remote)?;
            }
            Mode::Socks => {
                tcp_address(&self.local, true)?;
            }
            Mode::SocksRemote => {
                tcp_address(&self.remote, true)?;
            }
        }
        Ok(())
    }

    pub fn forwarding(&self) -> Result<(&'static str, String)> {
        self.validate()?;
        Ok(match self.mode {
            Mode::Local => (
                "-L",
                format!(
                    "{}:{}",
                    address(&self.local, true)?,
                    address(&self.remote, false)?
                ),
            ),
            Mode::Remote => (
                "-R",
                format!(
                    "{}:{}",
                    address(&self.remote, true)?,
                    address(&self.local, false)?
                ),
            ),
            Mode::Socks => ("-D", tcp_address(&self.local, true)?),
            Mode::SocksRemote => ("-R", tcp_address(&self.remote, true)?),
        })
    }
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        name.len() <= 100
            && name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "names/groups must start with a letter or digit and contain only letters, digits, '.', '_' or '-'"
    );
    Ok(())
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn valid_host(value: &str) -> bool {
    valid_token(value) || value.parse::<std::net::Ipv6Addr>().is_ok()
}

fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|port| port != 0)
}

pub fn address(value: &str, short: bool) -> Result<String> {
    if value.starts_with('/') {
        ensure!(
            value.len() <= 100 && !value.contains([':', '\0', '\n', '\r']),
            "Unix socket path must be absolute, at most 100 bytes, without ':' or control characters"
        );
        return Ok(value.to_owned());
    }
    tcp_address(value, short)
}

pub fn tcp_address(value: &str, short: bool) -> Result<String> {
    if valid_port(value) {
        ensure!(short, "address {value:?} requires host:port in this mode");
        return Ok(format!("127.0.0.1:{value}"));
    }
    if let Ok(socket) = value.parse::<std::net::SocketAddr>() {
        ensure!(socket.port() != 0, "port zero is not supported");
        return Ok(socket.to_string());
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && (valid_token(host) || host == "*")
        && valid_port(port)
    {
        return Ok(value.to_owned());
    }
    bail!(
        "invalid address {value:?}; expected host:port, [IPv6]:port, or an absolute Unix socket path"
    )
}

pub fn expand_env(input: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            output.push(character);
            continue;
        }
        let mut key = String::new();
        if chars.peek() == Some(&'{') {
            chars.next();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(character) => key.push(character),
                    None => bail!("unterminated environment variable in {input:?}"),
                }
            }
        } else {
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                key.push(chars.next().unwrap());
            }
        }
        if key.is_empty() {
            output.push('$');
            continue;
        }
        let (key, fallback) = key
            .split_once(":-")
            .map_or((key.as_str(), None), |(k, d)| (k, Some(d)));
        let value = lookup(key).filter(|value| !value.is_empty());
        output.push_str(
            &value
                .or_else(|| fallback.map(str::to_owned))
                .unwrap_or_default(),
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_modes_and_numeric_ports() {
        let text = r#"keep_alive = 30
[[tunnels]]
name = "dev"
host = "server"
local = 9000
remote = "db.internal:5432"
port = 2222
[[tunnels]]
name = "reverse"
host = "server"
local = "127.0.0.1:80"
remote = 8080
mode = "remote"
keep_alive = 0
[[tunnels]]
name = "proxy"
host = "server"
local = 1080
mode = "socks"
[[tunnels]]
name = "reverse-proxy"
host = "server"
remote = 1080
mode = "socks-remote"
"#;
        let config = Config::parse(text).unwrap();
        assert_eq!(
            config.tunnels[0].forwarding().unwrap(),
            ("-L", "127.0.0.1:9000:db.internal:5432".into())
        );
        assert_eq!(
            config.tunnels[1].forwarding().unwrap(),
            ("-R", "127.0.0.1:8080:127.0.0.1:80".into())
        );
        assert_eq!(
            config.tunnels[2].forwarding().unwrap(),
            ("-D", "127.0.0.1:1080".into())
        );
        assert_eq!(
            config.tunnels[3].forwarding().unwrap(),
            ("-R", "127.0.0.1:1080".into())
        );
        assert_eq!(config.tunnels[0].keep_alive, Some(30));
        assert_eq!(config.tunnels[1].keep_alive, Some(0));
    }

    #[test]
    fn rejects_typos_duplicates_and_injection() {
        let base =
            "[[tunnels]]\nname = 'dev'\nhost = 'server'\nlocal = 9000\nremote = 'localhost:80'\n";
        for bad in [
            format!("{base}{base}"),
            format!("{base}keap_alive = 30"),
            base.replace("'server'", "'-oProxyCommand=evil'"),
            base.replace("9000", "0"),
            base.replace("'dev'", "'bad*name'"),
        ] {
            assert!(Config::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ipv6_and_unix_sockets() {
        assert_eq!(address("[::1]:9000", true).unwrap(), "[::1]:9000");
        assert_eq!(
            address("/tmp/service.sock", false).unwrap(),
            "/tmp/service.sock"
        );
        assert!(address("::1:80", false).is_err());
        assert!(address("relative.sock", true).is_err());
    }

    #[test]
    fn environment_defaults_and_literal_names() {
        assert_eq!(
            expand_env("$HOST:${PORT:-22}/${EMPTY:-x}", |key| (key == "HOST")
                .then(|| "server".into()))
            .unwrap(),
            "server:22/x"
        );
        assert!(expand_env("${UNCLOSED", |_| None).is_err());
        let config = Config::parse(
            "[[tunnels]]\nname='literal'\nhost='${UNSET_LAB_HOST:-dev}'\nmode='socks'\nlocal=1080",
        )
        .unwrap();
        assert_eq!(config.tunnels[0].host, "dev");
    }
}
