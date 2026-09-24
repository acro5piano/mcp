# mcp

Turn any MCP server into a CLI.

Register a remote MCP server once, authorize it in your browser, and every tool it
exposes becomes a subcommand that prints raw JSON — so `jq` does the rest.

```console
$ mcp add linear --url https://mcp.linear.app/mcp
$ mcp linear auth
$ mcp linear list_issues | jq '.[].title'
```

## Install

```console
$ cargo install --path .
```

## Usage

### Register a server

```console
$ mcp add linear --url https://mcp.linear.app/mcp
```

For servers that authenticate with a plain API key instead of OAuth, skip `auth`
and pass the header directly:

```console
$ mcp add acme --url https://mcp.acme.dev/mcp --header 'Authorization: Bearer sk-...'
```

`mcp list` shows every registered server and whether it holds a live token;
`mcp remove <name>` drops the server and its credentials.

### Authorize

```console
$ mcp linear auth
```

This runs the full OAuth 2.1 browser flow, discovering everything from the server
URL alone:

1. The MCP endpoint's `401` names its protected-resource metadata (**RFC 9728**),
   which names the authorization server.
2. The authorization server's metadata is read from `.well-known`
   (**RFC 8414**, with OpenID Connect discovery as a fallback).
3. A client is registered on the fly (**RFC 7591**) — no client ID to copy.
4. The browser opens for consent; the code comes back to a loopback listener on a
   random port and is exchanged with **PKCE** (S256) and an **RFC 8707** resource
   indicator.

Tokens land in `~/.config/mcp/credentials.json` (mode `0600`) and are refreshed
automatically when they expire. `mcp <server> logout` forgets them.

Use `--no-browser` to print the URL instead of opening it — useful over SSH.

### Discover tools

```console
$ mcp linear
list_issues [--query <query>] [--limit <limit>] ...
    List issues in the workspace.
get_issue --id <id>
    Retrieve a single issue by identifier.
...

$ mcp linear tools           # the same list, as raw JSON
$ mcp linear get_issue --help  # one tool's input schema
```

Schemas are cached for a day (see [Schema cache](#schema-cache)), so listing
tools and resolving flags normally costs no round trip.

### Call tools

Arguments can be a JSON object:

```console
$ mcp linear get_issue '{"id":"ABC-123"}'
```

…or flags, coerced to the right JSON types using the tool's input schema
(`--include-archived` matches an `includeArchived` property; comma-separated
values become arrays; a boolean flag may stand alone):

```console
$ mcp linear list_issues --team ENG --limit 10 --labels bug,ui
```

…or JSON on stdin:

```console
$ echo '{"id":"ABC-123"}' | mcp linear get_issue -
```

### Output

Results print as raw JSON on stdout, ready for `jq`:

```console
$ mcp linear list_issues | jq -r '.[] | "\(.id)\t\(.title)"'
```

A result's `structuredContent` is preferred; otherwise text content is parsed as
JSON when it is JSON, and printed verbatim when it is not. A single-key envelope
such as `{"issues": [...]}` is unwrapped so the payload is at the top level. Pass
`--raw` for the untouched MCP result, including `content` block wrappers.

Progress goes to stderr and results to stdout, so pipes stay clean. A tool that
reports an error prints to stderr and exits non-zero.

### Schema cache

`tools/list` is needed to list tools and to coerce `--flag` values, but schemas
rarely change — so they are cached for **one day** under
`~/.config/mcp/cache/<server>.json`. A warm `mcp <server>` is a local operation
and needs no network or valid token at all.

The cache invalidates itself when the day is up, when the server's URL changes,
and when a tool you name is missing from the cached list (a sign it went stale
early). To force a re-read explicitly:

```console
$ mcp linear --refresh              # also works on any tool call
$ mcp cache clear linear            # drop one server's schemas
$ mcp cache clear                   # drop every server's
```

`MCP_CACHE_TTL` overrides the lifetime in seconds; `0` disables caching.

## Notes

- Transport is MCP **Streamable HTTP**, including `text/event-stream` responses.
- `MCP_CONFIG_DIR` overrides the config location (default `~/.config/mcp`).
- `add`, `remove`, `list` and `cache` are the built-in commands, so they shadow a
  server with one of those names.
- Reserved words `auth`, `logout`, `tools` and `help` shadow tools of the same
  name; reach those with `mcp <server> call <tool>`.

## Release

Update the version in `Cargo.toml`, refresh `Cargo.lock`, and commit the change.
Then push `main` and a matching `v*` tag:

```console
$ cargo check
$ git add Cargo.toml Cargo.lock
$ git commit -m "chore: release v0.2.0"
$ git push origin main
$ git tag -a v0.2.0 -m v0.2.0
$ git push origin v0.2.0
```

Pushing the tag runs the [release workflow](.github/workflows/release.yml), which
builds a static `x86_64-unknown-linux-musl` binary and publishes it as
`mcp-x86_64-unknown-linux-musl` on the GitHub release page. Check the result with:

```console
$ gh release view v0.2.0 --web
```

## License

MIT
