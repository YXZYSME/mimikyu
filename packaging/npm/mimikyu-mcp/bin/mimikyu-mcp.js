#!/usr/bin/env node
"use strict";

/**
 * Zero-dependency launcher for `mimikyu` (MCP stdio server & CLI).
 *
 * - Downloads the signed GitHub Release tarball for the current OS/CPU once into the
 *   user's cache directory, verifies its SHA-256 checksum against `SHA256SUMS`,
 *   and executes the native Rust `mimikyu` binary.
 * - Defaults to `mimikyu mcp` when invoked with zero arguments (standard MCP client config).
 * - Never writes diagnostic output to `stdout` so MCP JSON-RPC stdio frames remain clean.
 */

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync, spawn } = require("node:child_process");

const pkg = require("../package.json");
const VERSION = pkg.version;
const REPO = process.env.MIMIKYU_REPO || "YXZYSME/mimikyu";

function resolveTarget() {
  const key = `${process.platform}-${process.arch}`;
  switch (key) {
    case "darwin-arm64":
      return "aarch64-apple-darwin";
    case "darwin-x64":
      return "x86_64-apple-darwin";
    case "linux-arm64":
      return "aarch64-unknown-linux-gnu";
    case "linux-x64":
      return "x86_64-unknown-linux-gnu";
    default:
      throw new Error(
        `Unsupported platform/arch '${key}'. Supported targets: darwin-arm64, darwin-x64, linux-arm64, linux-x64.`
      );
  }
}

function resolveCacheRoot() {
  if (process.env.MIMIKYU_CACHE_DIR) {
    return process.env.MIMIKYU_CACHE_DIR;
  }
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Caches", "mimikyu", "npm");
  }
  const xdg = process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
  return path.join(xdg, "mimikyu", "npm");
}

async function fetchBuffer(url) {
  const res = await fetch(url, {
    redirect: "follow",
    headers: { "User-Agent": `mimikyu-mcp/${VERSION}` },
  });
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} fetching ${url}`);
  }
  const ab = await res.arrayBuffer();
  return Buffer.from(ab);
}

function parseExpectedSha256(sumsText, archiveName) {
  for (const rawLine of sumsText.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    const parts = line.split(/\s+/);
    if (parts.length >= 2) {
      const hash = parts[0].toLowerCase();
      const file = parts[1].replace(/^\*/, "");
      if (file === archiveName && /^[a-f0-9]{64}$/.test(hash)) {
        return hash;
      }
    }
  }
  throw new Error(`Entry for '${archiveName}' not found in SHA256SUMS`);
}

async function ensureBinary() {
  if (process.env.MIMIKYU_BIN && fs.existsSync(process.env.MIMIKYU_BIN)) {
    return process.env.MIMIKYU_BIN;
  }

  const target = resolveTarget();
  const installDir = path.join(resolveCacheRoot(), `v${VERSION}-${target}`);
  const binPath = path.join(installDir, "mimikyu");

  if (fs.existsSync(binPath)) {
    return binPath;
  }

  const archiveName = `mimikyu-${target}.tar.gz`;
  const baseUrl = `https://github.com/${REPO}/releases/download/v${VERSION}`;

  process.stderr.write(
    `[mimikyu-mcp] Downloading and verifying mimikyu v${VERSION} (${target})...\n`
  );

  const [sumsBuf, tarBuf] = await Promise.all([
    fetchBuffer(`${baseUrl}/SHA256SUMS`),
    fetchBuffer(`${baseUrl}/${archiveName}`),
  ]);

  const expectedSha = parseExpectedSha256(sumsBuf.toString("utf8"), archiveName);
  const actualSha = crypto.createHash("sha256").update(tarBuf).digest("hex");
  if (actualSha !== expectedSha) {
    throw new Error(
      `SHA-256 mismatch for ${archiveName}: expected ${expectedSha}, got ${actualSha}`
    );
  }

  fs.mkdirSync(installDir, { recursive: true });
  const tmpDir = fs.mkdtempSync(path.join(installDir, ".stage-"));
  try {
    const archivePath = path.join(tmpDir, archiveName);
    fs.writeFileSync(archivePath, tarBuf);
    execFileSync("tar", ["-xzf", archivePath, "-C", tmpDir], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    const extractedBin = path.join(tmpDir, "mimikyu");
    if (!fs.existsSync(extractedBin)) {
      throw new Error(`Archive ${archiveName} did not contain 'mimikyu' binary`);
    }
    fs.chmodSync(extractedBin, 0o755);
    fs.renameSync(extractedBin, binPath);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }

  return binPath;
}

async function main() {
  const binPath = await ensureBinary();
  const rawArgs = process.argv.slice(2);
  const args = rawArgs.length === 0 ? ["mcp"] : rawArgs;

  const child = spawn(binPath, args, { stdio: "inherit" });

  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => {
      if (!child.killed) {
        child.kill(sig);
      }
    });
  }

  child.on("error", (err) => {
    process.stderr.write(`[mimikyu-mcp] Failed to spawn ${binPath}: ${err.message}\n`);
    process.exit(1);
  });

  child.on("exit", (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
    } else {
      process.exit(code ?? 0);
    }
  });
}

main().catch((err) => {
  process.stderr.write(`[mimikyu-mcp] Error: ${err.message}\n`);
  process.exit(1);
});
