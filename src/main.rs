mod cache;
mod config;
mod mcp;
mod oauth;

use std::collections::BTreeMap;
use std::io::Read;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Map, Value};

use config::{Config, Server};
use mcp::{CallError, Client};

#[derive(Parser)]
#[command(
    name = "mcp",
    version,
    about = "Turn any MCP server into a CLI",
    after_help = "\
EXAMPLES:
    mcp add linear --url https://mcp.linear.app/mcp
    mcp linear auth                       # OAuth browser flow
    mcp linear                            # list the server's tools
    mcp linear list_issues                # call a tool, print raw JSON
    mcp linear get_issue '{\"id\":\"ABC-123\"}'
    mcp linear get_issue --id ABC-123     # same, using flags
    mcp linear list_issues | jq '.[].title'"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register an MCP server
    Add {
        /// Name used to invoke it, e.g. `mcp <name> <tool>`
        name: String,
        /// Streamable HTTP endpoint of the MCP server
        #[arg(long)]
        url: String,
        /// Extra header to send, e.g. --header 'Authorization: Bearer xxx'
        #[arg(long = "header", value_name = "NAME: VALUE")]
        headers: Vec<String>,
    },
    /// Remove a server and its stored credentials
    Remove { name: String },
    /// Manage the cached tool schemas
    Cache {
        #[command(subcommand)]
        action: CacheCommand,
    },
    /// List registered servers
    #[command(alias = "ls")]
    List,
    /// <server> [tool] [json-args] — run a tool on a registered server
    #[command(external_subcommand)]
    Server(Vec<String>),
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Forget cached schemas, for one server or for every server
    Clear {
        /// Server to clear; omit to clear them all
        server: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("mcp: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Add { name, url, headers } => {
            add(&name, &url, &headers)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Remove { name } => {
            remove(&name)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Cache {
            action: CacheCommand::Clear { server },
        } => {
            clear_cache(server.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::List => {
            list()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Server(args) => server(args).await,
    }
}

fn add(name: &str, url: &str, headers: &[String]) -> Result<()> {
    url::Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    let mut parsed = BTreeMap::new();
    for header in headers {
        let (key, value) = header
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid header {header:?}, expected 'Name: value'"))?;
        parsed.insert(key.trim().to_string(), value.trim().to_string());
    }
    let mut config = config::load_config()?;
    config.servers.insert(
        name.to_string(),
        Server {
            url: url.to_string(),
            headers: parsed,
        },
    );
    config::save_config(&config)?;
    eprintln!("Added {name} -> {url}");
    eprintln!("Next: mcp {name} auth");
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let mut config = config::load_config()?;
    if config.servers.remove(name).is_none() {
        bail!("no such server: {name}");
    }
    config::save_config(&config)?;
    config::delete_credentials(name)?;
    cache::clear(Some(name))?;
    eprintln!("Removed {name}");
    Ok(())
}

fn clear_cache(server: Option<&str>) -> Result<()> {
    let removed = cache::clear(server)?;
    match (server, removed) {
        (Some(name), 0) => eprintln!("No cached schemas for {name}"),
        (Some(name), _) => eprintln!("Cleared cached schemas for {name}"),
        (None, 0) => eprintln!("No cached schemas to clear"),
        (None, 1) => eprintln!("Cleared cached schemas for 1 server"),
        (None, n) => eprintln!("Cleared cached schemas for {n} servers"),
    }
    Ok(())
}

fn list() -> Result<()> {
    let config = config::load_config()?;
    if config.servers.is_empty() {
        eprintln!("No servers yet. Add one with: mcp add <name> --url <url>");
        return Ok(());
    }
    let credentials = config::load_all_credentials()?;
    let width = config.servers.keys().map(String::len).max().unwrap_or(0);
    for (name, server) in &config.servers {
        let status = match credentials.get(name) {
            Some(c) if c.access_token.is_some() && c.is_expired() && c.refresh_token.is_some() => {
                "authenticated (expired, will refresh)"
            }
            Some(c) if c.access_token.is_some() && c.is_expired() => "expired",
            Some(c) if c.access_token.is_some() => "authenticated",
            _ if !server.headers.is_empty() => "static headers",
            _ => "not authenticated",
        };
        println!("{name:<width$}  {}  [{status}]", server.url);
    }
    Ok(())
}

fn lookup(name: &str) -> Result<Server> {
    let config: Config = config::load_config()?;
    config.servers.get(name).cloned().ok_or_else(|| {
        let known: Vec<&str> = config.servers.keys().map(String::as_str).collect();
        if known.is_empty() {
            anyhow!("no such server: {name}. Add one with: mcp add {name} --url <url>")
        } else {
            anyhow!(
                "no such server: {name}. Known servers: {}",
                known.join(", ")
            )
        }
    })
}

async fn server(args: Vec<String>) -> Result<ExitCode> {
    let mut args = args.into_iter();
    let name = args.next().expect("external subcommand always has a name");
    let rest: Vec<String> = args.collect();
    let refresh = rest.iter().any(|a| a == "--refresh");
    let rest: Vec<String> = rest.into_iter().filter(|a| a != "--refresh").collect();

    match rest.first().map(String::as_str) {
        None => {
            print_tools(&name, false, refresh).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some("auth" | "login") => {
            let no_browser = rest.iter().any(|a| a == "--no-browser");
            auth(&name, no_browser).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some("logout") => {
            config::delete_credentials(&name)?;
            eprintln!("Cleared credentials for {name}");
            Ok(ExitCode::SUCCESS)
        }
        Some("tools") if rest.len() == 1 => {
            print_tools(&name, true, refresh).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some("--help" | "-h" | "help") => {
            eprintln!("usage: mcp {name} [tool] [json | --key value]\n");
            eprintln!("  mcp {name}            list the server's tools");
            eprintln!("  mcp {name} tools      the same list as raw JSON");
            eprintln!("  mcp {name} auth       authorize via the browser");
            eprintln!("  mcp {name} logout     forget the stored credentials");
            eprintln!("  mcp {name} --refresh  re-read the tool schemas, ignoring the cache\n");
            print_tools(&name, false, refresh).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some("call") => {
            let tool = rest
                .get(1)
                .ok_or_else(|| anyhow!("usage: mcp {name} call <tool> [json]"))?;
            call(&name, tool, &rest[2..], refresh).await
        }
        Some(tool) => {
            let tool = tool.to_string();
            call(&name, &tool, &rest[1..], refresh).await
        }
    }
}

/// Opens a session, refreshing or prompting for OAuth as needed.
async fn connect(name: &str) -> Result<Client> {
    let server = lookup(name)?;
    let http = reqwest::Client::new();
    let mut credentials = config::load_credentials(name)?;

    if credentials.access_token.is_some() && credentials.is_expired() {
        if let Some(refreshed) = oauth::refresh(&http, &credentials).await? {
            config::save_credentials(name, &refreshed)?;
            credentials = refreshed;
        }
    }

    let mut client = Client::new(
        server.url.clone(),
        server.headers.clone(),
        credentials.access_token.clone(),
    )?;

    match client.initialize().await {
        Ok(_) => Ok(client),
        Err(CallError::Other(error)) => Err(error),
        Err(CallError::Unauthorized(challenge)) => {
            // The token may have been revoked or expired early; try a refresh once.
            if let Some(refreshed) = oauth::refresh(&http, &credentials).await? {
                config::save_credentials(name, &refreshed)?;
                let mut client = Client::new(
                    server.url.clone(),
                    server.headers.clone(),
                    refreshed.access_token.clone(),
                )?;
                if client.initialize().await.is_ok() {
                    return Ok(client);
                }
            }
            let hint = match &challenge.www_authenticate {
                Some(header) => format!(" ({header})"),
                None => String::new(),
            };
            bail!("{name} requires authorization{hint}. Run: mcp {name} auth")
        }
    }
}

async fn auth(name: &str, no_browser: bool) -> Result<()> {
    let server = lookup(name)?;
    let http = reqwest::Client::new();
    let existing = config::load_credentials(name)?;

    // Probe the endpoint so the 401 can point us at the resource metadata (RFC 9728).
    let challenge = probe_challenge(&http, &server.url).await;

    let credentials = oauth::login(
        &http,
        &server.url,
        challenge.as_deref(),
        &existing,
        no_browser,
    )
    .await?;
    config::save_credentials(name, &credentials)?;
    eprintln!(
        "Authenticated. Credentials saved to {}",
        credentials_path_display()
    );
    eprintln!("Try: mcp {name}");
    Ok(())
}

fn credentials_path_display() -> String {
    config::config_dir()
        .map(|dir| dir.join("credentials.json").display().to_string())
        .unwrap_or_else(|_| "the config directory".to_string())
}

async fn probe_challenge(http: &reqwest::Client, url: &str) -> Option<String> {
    let response = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": mcp::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "mcp-cli", "version": env!("CARGO_PKG_VERSION") },
            },
        }))
        .send()
        .await
        .ok()?;
    response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Tool schemas for a server, served from the cache when it is still fresh —
/// which lets listing a server's tools stay a purely local operation.
async fn tools_for(name: &str, expect: Option<&str>, refresh: bool) -> Result<Vec<Value>> {
    let server = lookup(name)?;
    if !refresh {
        if let Some(tools) = cache::load(name, &server.url) {
            // A tool missing from a cached list usually just means it is stale.
            if expect.is_none_or(|tool| has_tool(&tools, tool)) {
                return Ok(tools);
            }
        }
    }
    let mut client = connect(name).await?;
    fetch_and_cache(name, &server.url, &mut client).await
}

/// The same, reusing a session we already opened.
async fn tools_with(
    client: &mut Client,
    name: &str,
    url: &str,
    expect: Option<&str>,
) -> Result<Vec<Value>> {
    if let Some(tools) = cache::load(name, url) {
        if expect.is_none_or(|tool| has_tool(&tools, tool)) {
            return Ok(tools);
        }
    }
    fetch_and_cache(name, url, client).await
}

async fn fetch_and_cache(name: &str, url: &str, client: &mut Client) -> Result<Vec<Value>> {
    let tools = client.list_tools().await?;
    cache::store(name, url, &tools)?;
    Ok(tools)
}

fn has_tool(tools: &[Value], tool: &str) -> bool {
    tools
        .iter()
        .any(|t| t.get("name").and_then(Value::as_str) == Some(tool))
}

async fn print_tools(name: &str, as_json: bool, refresh: bool) -> Result<()> {
    let tools = tools_for(name, None, refresh).await?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&tools)?);
        return Ok(());
    }

    if tools.is_empty() {
        eprintln!("{name} exposes no tools.");
        return Ok(());
    }

    for tool in &tools {
        let tool_name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        let required: Vec<&str> = tool
            .pointer("/inputSchema/required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let optional: Vec<&str> = tool
            .pointer("/inputSchema/properties")
            .and_then(Value::as_object)
            .map(|o| {
                o.keys()
                    .map(String::as_str)
                    .filter(|k| !required.contains(k))
                    .collect()
            })
            .unwrap_or_default();

        let mut signature = String::new();
        for key in &required {
            signature.push_str(&format!(" --{key} <{key}>"));
        }
        for key in &optional {
            signature.push_str(&format!(" [--{key} <{key}>]"));
        }
        println!("{tool_name}{signature}");

        let description = tool
            .get("description")
            .or_else(|| tool.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(first) = description.lines().find(|l| !l.trim().is_empty()) {
            println!("    {}", first.trim());
        }
    }
    eprintln!("\n{} tools. Full schemas: mcp {name} tools", tools.len());
    Ok(())
}

async fn call(name: &str, tool: &str, args: &[String], refresh: bool) -> Result<ExitCode> {
    let raw = args.iter().any(|a| a == "--raw");
    let args: Vec<String> = args.iter().filter(|a| *a != "--raw").cloned().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        return print_tool_help(name, tool, refresh).await;
    }

    let server = lookup(name)?;
    let mut client = connect(name).await?;

    let arguments = match args.split_first() {
        None => Value::Object(Map::new()),
        Some((first, rest)) if rest.is_empty() && !first.starts_with("--") => {
            parse_json_argument(first)?
        }
        Some(_) => {
            let tools = if refresh {
                fetch_and_cache(name, &server.url, &mut client).await?
            } else {
                tools_with(&mut client, name, &server.url, Some(tool)).await?
            };
            let schema = tools
                .iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(tool))
                .and_then(|t| t.get("inputSchema").cloned());
            parse_flags(&args, schema.as_ref())?
        }
    };

    let result = match client.call_tool(tool, arguments).await {
        Ok(result) => result,
        Err(CallError::Other(error)) => {
            // An unknown tool is a common typo; show what is available.
            if error.to_string().to_lowercase().contains("not found")
                || error.to_string().to_lowercase().contains("unknown tool")
            {
                // Refetch rather than trust a cache the server just contradicted.
                let names: Vec<String> = fetch_and_cache(name, &server.url, &mut client)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect();
                if !names.is_empty() {
                    return Err(error.context(format!("available tools: {}", names.join(", "))));
                }
            }
            return Err(error);
        }
        Err(error) => return Err(error.into()),
    };

    Ok(print_result(&result, raw))
}

/// Prints one tool's description and input schema.
async fn print_tool_help(name: &str, tool: &str, refresh: bool) -> Result<ExitCode> {
    let tools = tools_for(name, Some(tool), refresh).await?;
    let found = tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(tool))
        .ok_or_else(|| {
            let names: Vec<&str> = tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .collect();
            anyhow!("unknown tool: {tool}. Available: {}", names.join(", "))
        })?;
    if let Some(description) = found.get("description").and_then(Value::as_str) {
        eprintln!("{}\n", description.trim());
    }
    println!("{}", to_pretty(found.get("inputSchema").unwrap_or(found)));
    Ok(ExitCode::SUCCESS)
}

fn parse_json_argument(argument: &str) -> Result<Value> {
    let text = if argument == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .context("reading arguments from stdin")?;
        buffer
    } else {
        argument.to_string()
    };
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("arguments must be a JSON object, got: {}", text.trim()))?;
    if !value.is_object() {
        bail!("arguments must be a JSON object, got: {value}");
    }
    Ok(value)
}

/// Turns `--key value` / `--key=value` pairs into a JSON object, coercing values
/// with the tool's input schema when one is available.
fn parse_flags(args: &[String], schema: Option<&Value>) -> Result<Value> {
    let properties = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object);

    let mut object = Map::new();
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let Some(flag) = argument.strip_prefix("--") else {
            bail!("unexpected argument {argument:?}; pass tool arguments as JSON or --key value");
        };

        let (key, inline) = match flag.split_once('=') {
            Some((key, value)) => (key.to_string(), Some(value.to_string())),
            None => (flag.to_string(), None),
        };
        // `--include-archived` should reach a schema property named `includeArchived`.
        let key = match properties {
            Some(properties) => resolve_key(&key, properties),
            None => key,
        };

        let property = properties.and_then(|p| p.get(&key));
        if let (Some(properties), None) = (properties, property) {
            bail!(
                "unknown argument --{key}; this tool accepts: {}",
                properties
                    .keys()
                    .map(|k| format!("--{k}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let kind = property
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("string");

        let value = match inline {
            Some(value) => value,
            None => {
                // A boolean flag may stand alone: `--include-archived`.
                let next = args.get(index + 1);
                match next {
                    Some(next) if !next.starts_with("--") => {
                        index += 1;
                        next.clone()
                    }
                    _ if kind == "boolean" => "true".to_string(),
                    _ => bail!("--{key} needs a value"),
                }
            }
        };

        object.insert(key.clone(), coerce(&value, kind, property)?);
        index += 1;
    }
    Ok(Value::Object(object))
}

/// Matches a flag name against the schema's properties, ignoring case and the
/// `-`/`_` word separators, so `--include-archived` finds `includeArchived`.
fn resolve_key(key: &str, properties: &Map<String, Value>) -> String {
    if properties.contains_key(key) {
        return key.to_string();
    }
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| *c != '-' && *c != '_')
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let wanted = normalize(key);
    properties
        .keys()
        .find(|candidate| normalize(candidate) == wanted)
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

fn coerce(value: &str, kind: &str, property: Option<&Value>) -> Result<Value> {
    match kind {
        "number" => Ok(json!(value
            .parse::<f64>()
            .with_context(|| format!("expected a number, got {value:?}"))?)),
        "integer" => Ok(json!(value
            .parse::<i64>()
            .with_context(|| format!("expected an integer, got {value:?}"))?)),
        "boolean" => match value {
            "true" | "1" | "yes" => Ok(json!(true)),
            "false" | "0" | "no" => Ok(json!(false)),
            other => bail!("expected a boolean, got {other:?}"),
        },
        "array" => {
            if let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(value) {
                return Ok(parsed);
            }
            let item_kind = property
                .and_then(|p| p.pointer("/items/type"))
                .and_then(Value::as_str)
                .unwrap_or("string");
            let items: Result<Vec<Value>> = value
                .split(',')
                .map(|item| coerce(item.trim(), item_kind, None))
                .collect();
            Ok(Value::Array(items?))
        }
        "object" => serde_json::from_str(value)
            .with_context(|| format!("expected a JSON object, got {value:?}")),
        _ => Ok(Value::String(value.to_string())),
    }
}

/// Prints the tool result as raw JSON so it pipes straight into `jq`.
fn print_result(result: &Value, raw: bool) -> ExitCode {
    if raw {
        println!("{}", to_pretty(result));
        return exit_code(result);
    }

    if let Some(structured) = result.get("structuredContent") {
        if !structured.is_null() {
            println!("{}", to_pretty(unwrap_single_key(structured)));
            return exit_code(result);
        }
    }

    let Some(content) = result.get("content").and_then(Value::as_array) else {
        println!("{}", to_pretty(result));
        return exit_code(result);
    };

    let texts: Vec<&str> = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();

    // Non-text blocks (images, resources) have no plain rendering: emit them as-is.
    if texts.len() != content.len() {
        println!("{}", to_pretty(&json!(content)));
        return exit_code(result);
    }

    let parsed: Vec<Value> = texts
        .iter()
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .collect();

    let output = if parsed.len() == texts.len() && !parsed.is_empty() {
        match parsed.len() {
            1 => to_pretty(unwrap_single_key(&parsed[0])),
            _ => to_pretty(&json!(parsed)),
        }
    } else {
        texts.join("\n")
    };

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        eprintln!("{output}");
    } else {
        println!("{output}");
    }
    exit_code(result)
}

/// MCP servers often wrap a list in a single-key envelope such as
/// `{"issues": [...]}`; unwrap it so `jq` sees the payload directly.
fn unwrap_single_key(value: &Value) -> &Value {
    if let Some(object) = value.as_object() {
        if object.len() == 1 {
            if let Some(inner @ (Value::Array(_) | Value::Object(_))) = object.values().next() {
                return inner;
            }
        }
    }
    value
}

fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn exit_code(result: &Value) -> ExitCode {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "properties": {
                "id": { "type": "string" },
                "limit": { "type": "integer" },
                "includeArchived": { "type": "boolean" },
                "labels": { "type": "array", "items": { "type": "string" } },
                "filter": { "type": "object" }
            }
        })
    }

    fn flags(args: &[&str]) -> Result<Value> {
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse_flags(&owned, Some(&schema()))
    }

    #[test]
    fn coerces_flag_values_using_the_input_schema() {
        let parsed = flags(&["--id", "ABC-123", "--limit", "5", "--labels", "bug,ui"]).unwrap();
        assert_eq!(
            parsed,
            json!({ "id": "ABC-123", "limit": 5, "labels": ["bug", "ui"] })
        );
    }

    #[test]
    fn accepts_inline_values_and_kebab_case_keys() {
        let parsed = flags(&["--id=ABC-1", "--include-archived"]).unwrap();
        assert_eq!(parsed, json!({ "id": "ABC-1", "includeArchived": true }));
    }

    #[test]
    fn parses_json_valued_flags() {
        let parsed = flags(&["--filter", r#"{"state":"open"}"#, "--labels", r#"["a"]"#]).unwrap();
        assert_eq!(
            parsed,
            json!({ "filter": {"state":"open"}, "labels": ["a"] })
        );
    }

    #[test]
    fn rejects_unknown_and_valueless_flags() {
        assert!(flags(&["--nope", "x"])
            .unwrap_err()
            .to_string()
            .contains("unknown argument"));
        assert!(flags(&["--id"])
            .unwrap_err()
            .to_string()
            .contains("needs a value"));
    }

    #[test]
    fn treats_values_as_strings_without_a_schema() {
        let args = vec!["--anything".to_string(), "7".to_string()];
        assert_eq!(
            parse_flags(&args, None).unwrap(),
            json!({ "anything": "7" })
        );
    }

    #[test]
    fn unwraps_a_single_key_envelope_around_a_collection() {
        let wrapped = json!({ "issues": [{ "id": "MON-1" }] });
        assert_eq!(unwrap_single_key(&wrapped), &json!([{ "id": "MON-1" }]));
        // A lone scalar is the payload itself, not an envelope.
        let scalar = json!({ "result": "text" });
        assert_eq!(unwrap_single_key(&scalar), &scalar);
        let two = json!({ "a": [], "b": [] });
        assert_eq!(unwrap_single_key(&two), &two);
    }

    #[test]
    fn json_arguments_must_be_an_object() {
        assert!(parse_json_argument(r#"{"id":"A"}"#).is_ok());
        assert!(parse_json_argument("[1]").is_err());
        assert!(parse_json_argument("not json").is_err());
    }
}
