Create a rust cli tool which turns mcp server into cli.

interface:

```
mcp add linear --url https://mcp.linear.app/mcp

# OAuth browser flow (auto-discovers endpoints via RFC 9728 / RFC 8414)
mcp linear auth

# ... then save token somewhere

# Let's use it
mcp linear list_issues
# Returns raw json so that filterable with `jq`
# {
#   "id": "MON-3306",
#   "uuid": "4f41562d-f383-480a-9ac2-230b47d3a79c",
#   "title": "work on XXX",
#   "description": "## Background\n\n...",
#   "priority": {
#     "value": 0,
#     "name": "No priority"
#   },
#   "estimate": {
#     "value": 2,
#     "name": "2 Points"
#   },

# Pass args using json
mcp linear get_issue '{"id":"ABC-123"}'
```
