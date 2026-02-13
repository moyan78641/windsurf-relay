#!/usr/bin/env node

/**
 * WindSurf Relay MCP Client 启动器
 *
 * 查找预编译的 Rust 二进制并以 --mcp 模式启动。
 * stdin/stdout/stderr 直接透传给底层二进制。
 */

const { execFileSync } = require("child_process");
const path = require("path");
const fs = require("fs");
const os = require("os");

function getBinaryName() {
  const platform = os.platform();
  const arch = os.arch();

  const map = {
    "darwin-x64": "windsurf-relay-darwin-x64",
    "darwin-arm64": "windsurf-relay-darwin-arm64",
    "linux-x64": "windsurf-relay-linux-x64",
    "linux-arm64": "windsurf-relay-linux-arm64",
    "win32-x64": "windsurf-relay-win32-x64.exe",
    "win32-arm64": "windsurf-relay-win32-arm64.exe",
  };

  return map[`${platform}-${arch}`] || null;
}

function findBinary() {
  const name = getBinaryName();
  if (!name) {
    process.stderr.write(`Unsupported platform: ${os.platform()}-${os.arch()}\n`);
    process.exit(1);
  }

  const candidates = [
    path.join(__dirname, name),
    path.join(__dirname, "..", "bin", name),
  ];

  for (const p of candidates) {
    if (fs.existsSync(p)) return p;
  }

  process.stderr.write(
    `Binary not found: ${name}\n` +
    `Run 'npm rebuild windsurf-relay-cli' or download from GitHub Releases.\n` +
    `Searched:\n${candidates.map(p => `  - ${p}`).join("\n")}\n`
  );
  process.exit(1);
}

const binary = findBinary();
const args = ["--mcp", ...process.argv.slice(2)];

try {
  execFileSync(binary, args, { stdio: "inherit", env: process.env });
} catch (e) {
  process.exit(e.status || 1);
}
