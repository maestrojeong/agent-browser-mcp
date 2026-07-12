#!/usr/bin/env node
// Thin shim: exec the downloaded native binary, passing through argv + stdio.
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const native = path.join(__dirname, "agent-browser-native");
if (!fs.existsSync(native)) {
  console.error(
    "[agent-browser] native binary not found — reinstall, or build from source:\n" +
      "  cargo install --git https://github.com/maestrojeong/agent-browser-mcp ab-mcp"
  );
  process.exit(1);
}

const r = spawnSync(native, process.argv.slice(2), { stdio: "inherit" });
process.exit(r.status ?? 1);
