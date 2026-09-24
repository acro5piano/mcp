//! OAuth 2.1 for MCP: RFC 9728 protected-resource discovery, RFC 8414 authorization
//! server metadata, RFC 7591 dynamic client registration, PKCE, and RFC 8707
//! resource indicators.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::config::{now, Credentials};

pub struct AuthServer {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Option<String>,
}

/// Discovers the authorization server for an MCP endpoint.
///
/// Starts from the `resource_metadata` hint in a 401's `WWW-Authenticate` header
/// (RFC 9728), falls back to the well-known protected-resource paths, and finally
/// treats the MCP server's own origin as the authorization server.
pub async fn discover(
    http: &reqwest::Client,
    mcp_url: &str,
    www_authenticate: Option<&str>,
) -> Result<(AuthServer, String, Option<String>)> {
    let url = Url::parse(mcp_url).context("parsing server URL")?;
    let resource = canonical_resource(&url);

    let mut prm_candidates = Vec::new();
    if let Some(hint) = www_authenticate.and_then(parse_resource_metadata) {
        prm_candidates.push(hint);
    }
    prm_candidates.extend(well_known(&url, "oauth-protected-resource"));

    let mut issuer = None;
    let mut scopes = None;
    for candidate in prm_candidates {
        let Some(metadata) = fetch_json(http, &candidate).await else {
            continue;
        };
        scopes = metadata
            .get("scopes_supported")
            .and_then(Value::as_array)
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty());
        if let Some(first) = metadata
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
        {
            issuer = Some(first.to_string());
            break;
        }
    }

    // MCP fallback: the resource server doubles as the authorization server.
    let issuer = issuer.unwrap_or_else(|| origin(&url));
    let issuer_url = Url::parse(&issuer).with_context(|| format!("parsing issuer {issuer}"))?;

    let mut as_candidates = well_known(&issuer_url, "oauth-authorization-server");
    as_candidates.extend(well_known(&issuer_url, "openid-configuration"));

    for candidate in as_candidates {
        let Some(metadata) = fetch_json(http, &candidate).await else {
            continue;
        };
        let (Some(authorization_endpoint), Some(token_endpoint)) = (
            metadata
                .get("authorization_endpoint")
                .and_then(Value::as_str),
            metadata.get("token_endpoint").and_then(Value::as_str),
        ) else {
            continue;
        };
        let server_scopes = metadata
            .get("scopes_supported")
            .and_then(Value::as_array)
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty());
        return Ok((
            AuthServer {
                authorization_endpoint: authorization_endpoint.to_string(),
                token_endpoint: token_endpoint.to_string(),
                registration_endpoint: metadata
                    .get("registration_endpoint")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                scopes_supported: scopes.clone().or(server_scopes),
            },
            resource,
            Some(issuer),
        ));
    }

    // Last resort: the default endpoints from the OAuth 2.1 / MCP conventions.
    let base = issuer.trim_end_matches('/').to_string();
    Ok((
        AuthServer {
            authorization_endpoint: format!("{base}/authorize"),
            token_endpoint: format!("{base}/token"),
            registration_endpoint: Some(format!("{base}/register")),
            scopes_supported: scopes,
        },
        resource,
        Some(issuer),
    ))
}

/// RFC 8414 §3.1 / RFC 9728 §3.1: the path component is inserted after the
/// well-known segment, with the path-less form as a fallback.
fn well_known(url: &Url, suffix: &str) -> Vec<String> {
    let origin = origin(url);
    let path = url.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{suffix}{path}"));
        out.push(format!("{origin}{path}/.well-known/{suffix}"));
    }
    out.push(format!("{origin}/.well-known/{suffix}"));
    out
}

fn origin(url: &Url) -> String {
    let mut base = url.clone();
    base.set_path("");
    base.set_query(None);
    base.set_fragment(None);
    base.as_str().trim_end_matches('/').to_string()
}

/// RFC 8707 canonical resource URI: the server URL without query or fragment.
fn canonical_resource(url: &Url) -> String {
    let mut resource = url.clone();
    resource.set_query(None);
    resource.set_fragment(None);
    let s = resource.as_str();
    if s.ends_with('/') && resource.path() == "/" {
        s.trim_end_matches('/').to_string()
    } else {
        s.to_string()
    }
}

fn parse_resource_metadata(header: &str) -> Option<String> {
    let key = "resource_metadata=";
    let start = header.find(key)? + key.len();
    let rest = &header[start..];
    let value = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()?
    } else {
        rest.split(',').next()?.trim()
    };
    Some(value.to_string())
}

async fn fetch_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let response = http.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

/// RFC 7591 dynamic client registration.
async fn register_client(
    http: &reqwest::Client,
    endpoint: &str,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut body = json!({
        "client_name": "mcp-cli",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(scope) = scope {
        body["scope"] = json!(scope);
    }
    let response = http
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .context("registering OAuth client")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("client registration failed ({status}): {}", text.trim());
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing registration response: {}", text.trim()))?;
    let client_id = value
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("registration response has no client_id"))?
        .to_string();
    let client_secret = value
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((client_id, client_secret))
}

struct Pkce {
    verifier: String,
    challenge: String,
}

fn pkce() -> Pkce {
    let verifier = random_string(64);
    let digest = Sha256::digest(verifier.as_bytes());
    Pkce {
        challenge: URL_SAFE_NO_PAD.encode(digest),
        verifier,
    }
}

fn random_string(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// Runs the full browser authorization-code flow and returns fresh credentials.
pub async fn login(
    http: &reqwest::Client,
    mcp_url: &str,
    www_authenticate: Option<&str>,
    existing: &Credentials,
    no_browser: bool,
) -> Result<Credentials> {
    let (server, resource, _issuer) = discover(http, mcp_url, www_authenticate).await?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding local callback server")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let scope = server.scopes_supported.clone();

    // Reuse a previously registered client when the endpoints have not moved.
    let same_server = existing.token_endpoint.as_deref() == Some(server.token_endpoint.as_str());
    let (client_id, client_secret) = match (&existing.client_id, same_server) {
        (Some(id), true) => (id.clone(), existing.client_secret.clone()),
        _ => {
            let endpoint = server.registration_endpoint.clone().ok_or_else(|| {
                anyhow!(
                    "authorization server does not support dynamic client registration; \
                     no client_id available"
                )
            })?;
            register_client(http, &endpoint, &redirect_uri, scope.as_deref()).await?
        }
    };

    let pkce = pkce();
    let state = random_string(32);

    let mut auth_url =
        Url::parse(&server.authorization_endpoint).context("parsing authorization endpoint")?;
    {
        let mut q = auth_url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("state", &state);
        q.append_pair("code_challenge", &pkce.challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("resource", &resource);
        if let Some(scope) = &scope {
            q.append_pair("scope", scope);
        }
    }
    let auth_url = auth_url.to_string();

    eprintln!("Opening browser to authorize:\n{auth_url}\n");
    if !no_browser {
        let _ = webbrowser::open(&auth_url);
    }
    eprintln!("Waiting for the authorization callback on {redirect_uri} …");

    let code = wait_for_callback(listener, &state).await?;

    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code),
        ("redirect_uri".to_string(), redirect_uri.clone()),
        ("client_id".to_string(), client_id.clone()),
        ("code_verifier".to_string(), pkce.verifier),
        ("resource".to_string(), resource.clone()),
    ];
    if let Some(secret) = &client_secret {
        form.push(("client_secret".to_string(), secret.clone()));
    }

    let mut creds = token_request(http, &server.token_endpoint, &form).await?;
    creds.client_id = Some(client_id);
    creds.client_secret = client_secret;
    creds.authorization_endpoint = Some(server.authorization_endpoint);
    creds.token_endpoint = Some(server.token_endpoint);
    creds.registration_endpoint = server.registration_endpoint;
    creds.resource = Some(resource);
    if creds.scope.is_none() {
        creds.scope = scope;
    }
    Ok(creds)
}

/// Exchanges a refresh token. Returns `None` when there is nothing to refresh with.
pub async fn refresh(
    http: &reqwest::Client,
    existing: &Credentials,
) -> Result<Option<Credentials>> {
    let (Some(refresh_token), Some(token_endpoint), Some(client_id)) = (
        existing.refresh_token.as_ref(),
        existing.token_endpoint.as_ref(),
        existing.client_id.as_ref(),
    ) else {
        return Ok(None);
    };

    let mut form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.clone()),
        ("client_id".to_string(), client_id.clone()),
    ];
    if let Some(secret) = &existing.client_secret {
        form.push(("client_secret".to_string(), secret.clone()));
    }
    if let Some(resource) = &existing.resource {
        form.push(("resource".to_string(), resource.clone()));
    }
    if let Some(scope) = &existing.scope {
        form.push(("scope".to_string(), scope.clone()));
    }

    let mut creds = match token_request(http, token_endpoint, &form).await {
        Ok(creds) => creds,
        Err(_) => return Ok(None),
    };
    // Servers may omit a rotated refresh token; keep the one we have.
    if creds.refresh_token.is_none() {
        creds.refresh_token = existing.refresh_token.clone();
    }
    creds.client_id = existing.client_id.clone();
    creds.client_secret = existing.client_secret.clone();
    creds.authorization_endpoint = existing.authorization_endpoint.clone();
    creds.token_endpoint = existing.token_endpoint.clone();
    creds.registration_endpoint = existing.registration_endpoint.clone();
    creds.resource = existing.resource.clone();
    if creds.scope.is_none() {
        creds.scope = existing.scope.clone();
    }
    Ok(Some(creds))
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(String, String)],
) -> Result<Credentials> {
    let response = http
        .post(endpoint)
        .form(form)
        .send()
        .await
        .context("requesting token")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token request failed ({status}): {}", text.trim());
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing token response: {}", text.trim()))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token response has no access_token"))?
        .to_string();
    Ok(Credentials {
        access_token: Some(access_token),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_at: value
            .get("expires_in")
            .and_then(Value::as_u64)
            .map(|secs| now() + secs),
        scope: value
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..Default::default()
    })
}

/// Serves the loopback redirect URI until the authorization code arrives.
async fn wait_for_callback(listener: TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await.context("accepting callback")?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 2048];
        // The request line is all we need; stop as soon as the headers end.
        loop {
            let read = socket.read(&mut chunk).await.context("reading callback")?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() > 64 * 1024 {
                break;
            }
        }

        let request = String::from_utf8_lossy(&buffer);
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            respond(&mut socket, "Bad request.").await;
            continue;
        };

        let url = Url::parse(&format!("http://localhost{target}"))?;
        if url.path() != "/callback" {
            respond(&mut socket, "Waiting for the authorization callback…").await;
            continue;
        }

        let params: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let get = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };

        if let Some(error) = get("error") {
            let description = get("error_description").unwrap_or_default();
            respond(&mut socket, &format!("Authorization failed: {error}")).await;
            bail!("authorization failed: {error} {description}");
        }

        match (get("code"), get("state")) {
            (Some(code), Some(returned)) if returned == state => {
                respond(&mut socket, "Authorized. You can close this tab.").await;
                return Ok(code);
            }
            (Some(_), _) => {
                respond(&mut socket, "State mismatch.").await;
                bail!("authorization state mismatch — possible CSRF, aborting");
            }
            _ => {
                respond(&mut socket, "Missing authorization code.").await;
            }
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>mcp</title>\
         <body style=\"font:16px system-ui;padding:3rem\">{message}</body>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_resource_metadata_from_challenge() {
        let header = r#"Bearer realm="OAuth", resource_metadata="https://mcp.linear.app/.well-known/oauth-protected-resource/mcp", scope="read write""#;
        assert_eq!(
            parse_resource_metadata(header).as_deref(),
            Some("https://mcp.linear.app/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(parse_resource_metadata("Bearer realm=\"OAuth\""), None);
    }

    #[test]
    fn builds_well_known_candidates_with_path_insertion() {
        let url = Url::parse("https://example.com/tenant/mcp").unwrap();
        assert_eq!(
            well_known(&url, "oauth-protected-resource"),
            vec![
                "https://example.com/.well-known/oauth-protected-resource/tenant/mcp",
                "https://example.com/tenant/mcp/.well-known/oauth-protected-resource",
                "https://example.com/.well-known/oauth-protected-resource",
            ]
        );
    }

    #[test]
    fn well_known_for_root_issuer_has_no_path_variants() {
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(
            well_known(&url, "openid-configuration"),
            vec!["https://example.com/.well-known/openid-configuration"]
        );
    }

    #[test]
    fn canonical_resource_drops_query_and_fragment() {
        let url = Url::parse("https://mcp.example.com/mcp?x=1#f").unwrap();
        assert_eq!(canonical_resource(&url), "https://mcp.example.com/mcp");
        let root = Url::parse("https://mcp.example.com/").unwrap();
        assert_eq!(canonical_resource(&root), "https://mcp.example.com");
    }

    #[test]
    fn pkce_challenge_is_the_s256_of_the_verifier() {
        let pkce = pkce();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        assert_eq!(pkce.verifier.len(), 64);
    }
}
