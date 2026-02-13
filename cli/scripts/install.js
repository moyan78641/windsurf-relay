#!/usr/bin/env node

/**
 * postinstall 脚本 — 下载预编译的 Rust 二进制
 *
 * 从 GitHub Releases 下载对应平台的二进制文件。
 * 如果下载失败，提示用户手动下载或从源码编译。
 */

const https = require("https");
const fs = require("fs");
const path = require("path");
const os = require("os");
const { execSync } = require("child_process");

// ─── 配置 ──────────────────────────────────────────────────

const REPO = "moyan78641/windsurf-relay";
const VERSION = require("../package.json").version;

// ─── 平台映射 ──────────────────────────────────────────────

function getAssetName() {
  const platform = os.platform();
  const arch = os.arch();

  const map = {
    "darwin-x64": "windsurf-relay-darwin-x64.tar.gz",
    "darwin-arm64": "windsurf-relay-darwin-arm64.tar.gz",
    "linux-x64": "windsurf-relay-linux-x64.tar.gz",
    "linux-arm64": "windsurf-relay-linux-arm64.tar.gz",
    "win32-x64": "windsurf-relay-win32-x64.zip",
    "win32-arm64": "windsurf-relay-win32-arm64.zip",
  };

  return map[`${platform}-${arch}`] || null;
}

function getBinaryName() {
  const platform = os.platform();
  const arch = os.arch();
  const ext = platform === "win32" ? ".exe" : "";
  return `windsurf-relay-${platform}-${arch}${ext}`;
}

// ─── 下载 ──────────────────────────────────────────────────

function download(url) {
  return new Promise((resolve, reject) => {
    const request = (u) => {
      https.get(u, { headers: { "User-Agent": "windsurf-relay-cli" } }, (res) => {
        // 跟随重定向
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          request(res.headers.location);
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error(`HTTP ${res.statusCode}`));
          return;
        }
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      }).on("error", reject);
    };
    request(url);
  });
}

// ─── 主逻辑 ────────────────────────────────────────────────

async function main() {
  const assetName = getAssetName();
  if (!assetName) {
    console.warn(`[windsurf-relay] Unsupported platform: ${os.platform()}-${os.arch()}`);
    console.warn("[windsurf-relay] Please build from source: cargo build --release");
    return;
  }

  const binDir = path.join(__dirname, "..", "bin");
  const binaryName = getBinaryName();
  const binaryPath = path.join(binDir, binaryName);

  // 已存在则跳过
  if (fs.existsSync(binaryPath)) {
    console.log(`[windsurf-relay] Binary already exists: ${binaryName}`);
    return;
  }

  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${assetName}`;
  console.log(`[windsurf-relay] Downloading ${assetName}...`);

  try {
    const data = await download(url);

    if (!fs.existsSync(binDir)) {
      fs.mkdirSync(binDir, { recursive: true });
    }

    if (assetName.endsWith(".tar.gz")) {
      // 解压 tar.gz
      const tmpFile = path.join(os.tmpdir(), assetName);
      fs.writeFileSync(tmpFile, data);
      execSync(`tar -xzf "${tmpFile}" -C "${binDir}"`, { stdio: "pipe" });
      fs.unlinkSync(tmpFile);
    } else if (assetName.endsWith(".zip")) {
      // 解压 zip（Windows）
      const tmpFile = path.join(os.tmpdir(), assetName);
      fs.writeFileSync(tmpFile, data);
      try {
        execSync(`powershell -Command "Expand-Archive -Path '${tmpFile}' -DestinationPath '${binDir}' -Force"`, { stdio: "pipe" });
      } catch {
        execSync(`unzip -o "${tmpFile}" -d "${binDir}"`, { stdio: "pipe" });
      }
      fs.unlinkSync(tmpFile);
    }

    // 设置可执行权限
    if (os.platform() !== "win32" && fs.existsSync(binaryPath)) {
      fs.chmodSync(binaryPath, 0o755);
    }

    console.log(`[windsurf-relay] Installed: ${binaryName}`);
  } catch (err) {
    console.warn(`[windsurf-relay] Download failed: ${err.message}`);
    console.warn(`[windsurf-relay] You can:`);
    console.warn(`  1. Download manually from: https://github.com/${REPO}/releases`);
    console.warn(`  2. Build from source: cargo build --release`);
    console.warn(`  3. Place the binary at: ${binaryPath}`);
  }
}

main();
