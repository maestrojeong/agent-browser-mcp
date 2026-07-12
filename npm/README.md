# agent-browser-mcp

Stealth MCP browser server — a single Rust binary, distributed over npm.

```bash
npm i -g agent-browser-mcp
agent-browser --port 9321            # HTTP MCP at http://127.0.0.1:9321/mcp
# or stdio:
agent-browser
```

`postinstall` downloads the prebuilt native binary for your platform from the
GitHub Release (macOS-arm64 / Linux-x64). No Node/Playwright runtime needed —
it just runs the ~4 MB Rust binary.

## Use with an MCP client

```jsonc
{ "mcpServers": { "agent-browser": {
  "command": "agent-browser"                       // stdio
} } }
// or HTTP: run `agent-browser --port 9321` and point the client at
//   http://127.0.0.1:9321/mcp
```

## Flags (patchright-compatible)

```
--port <n>   --host <h>   --user-data-dir <path>
--headless | --headed     --connect <port|url>     --stealth
```

Full docs: https://github.com/maestrojeong/agent-browser-mcp
