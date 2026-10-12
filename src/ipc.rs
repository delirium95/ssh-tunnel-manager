use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::config::Tunnel;

const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    List,
    Open { tunnels: Vec<Tunnel> },
    Close { names: Vec<String> },
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Connecting,
    Open,
    Reconnecting,
    Failed,
    Closed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Open => "open",
            Self::Reconnecting => "reconnecting",
            Self::Failed => "failed",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub tunnel: Tunnel,
    pub status: Status,
    pub connected_at: Option<u64>,
    pub retries: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub pid: u32,
    pub tunnels: Vec<Snapshot>,
    pub errors: Vec<String>,
}

pub fn read<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= MAX_FRAME,
        "invalid IPC frame length {length}"
    );
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).context("invalid IPC message")
}

pub fn write(value: &impl Serialize, writer: &mut impl Write) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    ensure!(body.len() <= MAX_FRAME, "IPC message is too large");
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

pub fn call(socket: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to daemon at {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write(request, &mut stream)?;
    read(&mut stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_large_truncated_and_unknown_frames() {
        assert!(read::<Request>(&mut &(u32::MAX.to_be_bytes())[..]).is_err());
        assert!(read::<Request>(&mut &[0, 0, 0, 4, b'{'][..]).is_err());
        let mut bytes = Vec::new();
        write(&Request::Ping, &mut bytes).unwrap();
        assert!(matches!(
            read::<Request>(&mut bytes.as_slice()).unwrap(),
            Request::Ping
        ));
        assert!(serde_json::from_str::<Request>(r#"{"op":"run_shell","command":"id"}"#).is_err());
    }
}
