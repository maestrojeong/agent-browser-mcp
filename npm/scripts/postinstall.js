#!/usr/bin/env node
// Downloads the prebuilt agent-browser native binary matching this platform
// from the matching GitHub Release, so `npm i -g agent-browser-mcp` "just works"
// like a native package (esbuild/biome-style distribution for a Rust binary).
const fs = require("node:fs");
const path = require("node:path");

const VERSION = require("../package.json").version;
const REPO = "maestrojeong/agent-browser-mcp";

// process.platform / arch -> release asset name (see .github/workflows/release.yml)
const ASSETS = {
  "darwin-arm64": "agent-browser-macos-arm64",
  "linux-x64": "agent-browser-linux-x64",
};

async function main() {
  const key = `${process.platform}-${process.arch}`;
  const asset = ASSETS[key];
  if (!asset) {
    console.error(
      `[agent-browser] no prebuilt binary for ${key}.\n` +
        `Build from source instead: cargo install --git https://github.com/${REPO} ab-mcp`
    );
    process.exit(0); // don't hard-fail the install
  }

  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;
  const outDir = path.join(__dirname, "..", "bin");
  const out = path.join(outDir, "agent-browser-native");
  fs.mkdirSync(outDir, { recursive: true });

  console.log(`[agent-browser] downloading ${asset} v${VERSION} …`);
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    console.error(`[agent-browser] download failed: ${res.status} ${url}`);
    process.exit(0);
  }
  const buf = Buffer.from(await res.arrayBuffer());
  fs.writeFileSync(out, buf);
  fs.chmodSync(out, 0o755);
  console.log(`[agent-browser] installed native binary (${buf.length} bytes).`);
}

main().catch((e) => {
  console.error("[agent-browser] postinstall error:", e.message);
  process.exit(0);
});
