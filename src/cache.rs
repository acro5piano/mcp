//! On-disk cache of each server's tool schemas.
//!
//! `tools/list` is needed to resolve `--flag` arguments and to list tools, but a
//! server's schemas rarely change. Caching them keeps a flag-style call to one
//! round trip and makes plain `mcp <server>` a local operation.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{config_dir, now};

/// How long a cached schema stays usable, in seconds.
pub const DEFAULT_TTL: u64 = 86_400;

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Unix seconds at which the schemas were fetched.
    fetched_at: u64,
    /// The URL they came from, so re-pointing a server invalidates them.
    url: String,
    tools: Vec<Value>,
}

/// `MCP_CACHE_TTL` overrides the lifetime; `0` disables the cache entirely.
fn ttl() -> u64 {
    std::env::var("MCP_CACHE_TTL")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TTL)
}

fn is_fresh(entry: &Entry, url: &str, ttl: u64, now: u64) -> bool {
    entry.url == url && now.saturating_sub(entry.fetched_at) < ttl
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("cache"))
}

/// Server names reach the filesystem, so keep them to a single safe segment.
fn file_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.json")
}

fn path(name: &str) -> Result<PathBuf> {
    Ok(cache_dir()?.join(file_name(name)))
}

/// Returns the cached schemas, or `None` on a miss, an expiry, a URL change, or
/// anything unreadable — a bad cache should never be worse than no cache.
pub fn load(name: &str, url: &str) -> Option<Vec<Value>> {
    let ttl = ttl();
    if ttl == 0 {
        return None;
    }
    let text = fs::read_to_string(path(name).ok()?).ok()?;
    let entry: Entry = serde_json::from_str(&text).ok()?;
    is_fresh(&entry, url, ttl, now()).then_some(entry.tools)
}

pub fn store(name: &str, url: &str, tools: &[Value]) -> Result<()> {
    if ttl() == 0 {
        return Ok(());
    }
    let path = path(name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let entry = Entry {
        fetched_at: now(),
        url: url.to_string(),
        tools: tools.to_vec(),
    };
    fs::write(&path, serde_json::to_string(&entry)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Drops one server's cached schemas, or every server's when `name` is `None`.
/// Returns how many entries were removed.
pub fn clear(name: Option<&str>) -> Result<usize> {
    if let Some(name) = name {
        let path = path(name)?;
        return match fs::remove_file(&path) {
            Ok(()) => Ok(1),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        };
    }

    let dir = cache_dir()?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    let mut removed = 0;
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == "json") {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fetched_at: u64) -> Entry {
        Entry {
            fetched_at,
            url: "https://example.com/mcp".to_string(),
            tools: vec![],
        }
    }

    #[test]
    fn an_entry_is_fresh_until_the_ttl_elapses() {
        let entry = entry(1_000);
        let url = "https://example.com/mcp";
        assert!(is_fresh(&entry, url, DEFAULT_TTL, 1_000));
        assert!(is_fresh(&entry, url, DEFAULT_TTL, 1_000 + DEFAULT_TTL - 1));
        assert!(!is_fresh(&entry, url, DEFAULT_TTL, 1_000 + DEFAULT_TTL));
    }

    #[test]
    fn a_changed_url_invalidates_the_entry() {
        assert!(!is_fresh(
            &entry(1_000),
            "https://other.example/mcp",
            DEFAULT_TTL,
            1_000
        ));
    }

    #[test]
    fn a_clock_moving_backwards_does_not_expire_the_entry() {
        assert!(is_fresh(
            &entry(5_000),
            "https://example.com/mcp",
            DEFAULT_TTL,
            1_000
        ));
    }

    #[test]
    fn server_names_become_a_single_safe_path_segment() {
        assert_eq!(file_name("linear"), "linear.json");
        assert_eq!(file_name("../../etc/passwd"), "______etc_passwd.json");
    }
}
