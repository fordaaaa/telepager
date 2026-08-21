#!/usr/bin/env node
"use strict";

// telepager ships as a prebuilt binary. The normal path is an optional
// dependency per platform, which npm picks the right one of and which works
// with --ignore-scripts. This script is the fallback for when that didn't
// happen: it downloads the binary from GitHub Releases and checksums it.
//
// It is deliberately impossible for this to fail an install. If nothing works
// here, `telepager` says so clearly the first time it's run.

const fs = require("fs");
const path = require("path");
const https = require("https");
const crypto = require("crypto");

const pkg = require("./package.json");
const VERSION = pkg.version;
const REPO = "fordaaaa/telepager";

const IS_WINDOWS = process.platform === "win32";
const BIN_NAME = IS_WINDOWS ? "telepager.exe" : "telepager";
const BIN_DIR = path.join(__dirname, "bin");
const FALLBACK_PATH = path.join(BIN_DIR, BIN_NAME);

const TARGETS = {
  "darwin arm64": "aarch64-apple-darwin",
  "darwin x64": "x86_64-apple-darwin",
  "linux x64": "x86_64-unknown-linux-gnu",
  "linux arm64": "aarch64-unknown-linux-gnu",
  "win32 x64": "x86_64-pc-windows-msvc",
};

const MISSING_MESSAGE =
  "[telepager] no binary for this platform (" +
  process.platform +
  " " +
  process.arch +
  ").\n" +
  "  Supported: " +
  Object.keys(TARGETS).join(", ") +
  "\n" +
  "  If yours is on that list, the download may have been blocked. Try:\n" +
  "    npm rebuild telepager\n" +
  "  Or build from source with `cargo build --release` and put the binary on your PATH.";

/** The per-platform package npm should have installed for this machine. */
function platformPackage() {
  return "telepager-" + process.platform + "-" + process.arch;
}

/**
 * Where the binary actually is, or null. Checks the optional dependency
 * first, then anything a previous fallback download left behind.
 */
function resolveBinary() {
  const override = process.env.TELEPAGER_BINARY;
  if (override && fs.existsSync(override)) return override;

  try {
    const manifest = require.resolve(platformPackage() + "/package.json");
    const candidate = path.join(path.dirname(manifest), "bin", BIN_NAME);
    if (fs.existsSync(candidate)) return candidate;
  } catch (_) {
    // the optional dependency isn't installed, which is what the fallback is for
  }

  if (fs.existsSync(FALLBACK_PATH)) return FALLBACK_PATH;
  return null;
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
        if (status !== 200) {
          res.resume();
          return reject(new Error("unexpected HTTP status " + status));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

async function download() {
  const triple = TARGETS[process.platform + " " + process.arch];
  if (!triple) throw new Error("unsupported platform");

  const name = "telepager-" + triple + (IS_WINDOWS ? ".exe" : "");
  const base = "https://github.com/" + REPO + "/releases/download/v" + VERSION;

  const binary = await get(base + "/" + name);
  const expected = (await get(base + "/" + name + ".sha256"))
    .toString("utf8")
    .trim()
    .split(/\s+/)[0];

  const actual = crypto.createHash("sha256").update(binary).digest("hex");
  if (actual !== expected) {
    throw new Error("checksum mismatch, refusing to install");
  }

  fs.mkdirSync(BIN_DIR, { recursive: true });
  fs.writeFileSync(FALLBACK_PATH, binary);
  if (!IS_WINDOWS) fs.chmodSync(FALLBACK_PATH, 0o755);
}

async function main() {
  if (resolveBinary()) return; // the optional dependency did its job

  try {
    await download();
    console.log("[telepager] installed the binary for " + process.platform + " " + process.arch);
  } catch (err) {
    // never fail the install over this — the CLI explains itself when run
    console.warn("[telepager] could not fetch a binary now (" + err.message + ").");
    console.warn("[telepager] run `npm rebuild telepager` once you're online.");
  }
}

module.exports = { resolveBinary, platformPackage, MISSING_MESSAGE };

if (require.main === module) {
  main();
}
