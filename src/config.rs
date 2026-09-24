use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// A server registered with `mcp add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub url: String,
    /// Static headers, e.g. for servers using a plain API key instead of OAuth.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: BTreeMap<String, Server>,
}

/// OAuth state for one server: the client we registered plus the tokens we hold.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Unix seconds. `None` means the token was issued without an expiry.
    pub expires_at: Option<u64>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
    /// RFC 8707 resource indicator the tokens were minted for.
    pub resource: Option<String>,
    pub scope: Option<String>,
}

impl Credentials {
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            // Refresh a minute early so a token does not die mid-request.
            Some(exp) => now() + 60 >= exp,
            None => false,
        }
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("MCP_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::config_dir().ok_or_else(|| anyhow!("cannot determine config directory"))?;
    Ok(base.join("mcp"))
}

fn servers_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("servers.json"))
}

fn credentials_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("credentials.json"))
}

fn read_json<T: Default + for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(T::default()),
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &PathBuf, value: &T, private: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{body}\n")).with_context(|| format!("writing {}", path.display()))?;
    if private {
        set_private(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_path: &PathBuf) -> Result<()> {
    Ok(())
}

pub fn load_config() -> Result<Config> {
    read_json(&servers_path()?)
}

pub fn save_config(config: &Config) -> Result<()> {
    write_json(&servers_path()?, config, false)
}

pub fn load_all_credentials() -> Result<BTreeMap<String, Credentials>> {
    read_json(&credentials_path()?)
}

pub fn load_credentials(name: &str) -> Result<Credentials> {
    Ok(load_all_credentials()?.remove(name).unwrap_or_default())
}

pub fn save_credentials(name: &str, creds: &Credentials) -> Result<()> {
    let mut all = load_all_credentials()?;
    all.insert(name.to_string(), creds.clone());
    write_json(&credentials_path()?, &all, true)
}

pub fn delete_credentials(name: &str) -> Result<()> {
    let mut all = load_all_credentials()?;
    if all.remove(name).is_some() {
        write_json(&credentials_path()?, &all, true)?;
    }
    Ok(())
}
