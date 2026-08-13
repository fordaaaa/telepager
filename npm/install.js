#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const https = require("https");
const crypto = require("crypto");

const pkg = require("./package.json");
const VERSION = pkg.version;
const REPO = "fordaaaa/telepager";

const BIN_DIR = path.join(__dirname, "bin");
const IS_WINDOWS = process.platform === "win32";
const OUT_PATH = path.join(BIN_DIR, IS_WINDOWS ? "telepager.exe" : "telepager");

const TARGETS = {
  "darwin arm64": "aarch64-apple-darwin",
  "darwin x64": "x86_64-apple-darwin",
  "linux x64": "x86_64-unknown-linux-gnu",
  "linux arm64": "aarch64-unknown-linux-gnu",
  "win32 x64": "x86_64-pc-windows-msvc",
};

function fail(message) {
  console.error("\n[telepager] " + message + "\n");
  process.exit(1);
}

function assetName() {
  const key = `${process.platform} ${process.arch}`;
  const triple = TARGETS[key];
  if (!triple) {
    fail(
      `Unsupported platform/arch: ${key}. ` +
        `Supported: ${Object.keys(TARGETS).join(", ")}. ` +
        `Build from source instead with \`cargo build --release\`.`
    );
  }
  return `telepager-${triple}` + (IS_WINDOWS ? ".exe" : "");
}

function get(url, redirects) {
  redirects = redirects || 0;
  return new Promise((resolve, reject) => {
    if (redirects > 10) return reject(new Error("too many redirects"));
    https
      .get(url, { headers: { "User-Agent": "telepager-installer" } }, (res) => {
        const status = res.statusCode || 0;
        if (status >= 300 && status < 400 && res.headers.location) {
          res.resume();
          return resolve(get(res.headers.location, redirects + 1));
        }
        if (status === 404) {
          res.resume();
          return reject(Object.assign(new Error("not found (404)"), { code: 404 }));
        }
        if (status !== 200) {
          res.resume();
          return reject(new Error(`unexpected HTTP status ${status}`));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

async function main() {
  fs.mkdirSync(BIN_DIR, { recursive: true });

  // for working on telepager itself, before any release exists
  const override = process.env.TELEPAGER_BINARY;
  if (override) {
    if (!fs.existsSync(override)) fail(`TELEPAGER_BINARY set but nothing at ${override}`);
    fs.copyFileSync(override, OUT_PATH);
    if (!IS_WINDOWS) fs.chmodSync(OUT_PATH, 0o755);
    console.log(`[telepager] using local binary from ${override}`);
    return;
  }

  const name = assetName();
  const base = `https://github.com/${REPO}/releases/download/v${VERSION}`;
  console.log(`[telepager] downloading ${name}`);

  let binary, expected;
  try {
    binary = await get(`${base}/${name}`);
    expected = (await get(`${base}/${name}.sha256`)).toString("utf8").trim().split(/\s+/)[0];
  } catch (err) {
    if (err && err.code === 404) {
      fail(
        `No prebuilt binary for v${VERSION} on this platform.\n` +
          `  The release may not be published yet: https://github.com/${REPO}/releases\n` +
          `  You can build from source with \`cargo build --release\` and set\n` +
          `  TELEPAGER_BINARY to the resulting path.`
      );
    }
    fail(`download failed: ${err.message}`);
  }

  const actual = crypto.createHash("sha256").update(binary).digest("hex");
  if (actual !== expected) {
    fail(`checksum mismatch, refusing to install\n  expected ${expected}\n  got      ${actual}`);
  }

  fs.writeFileSync(OUT_PATH, binary);
  if (!IS_WINDOWS) fs.chmodSync(OUT_PATH, 0o755);
  console.log(`[telepager] installed to ${OUT_PATH}`);
}

main().catch((err) => fail(err.message));
