#!/usr/bin/env node
"use strict";

// stdio has to pass straight through, stdin/stdout are the mcp transport

const path = require("path");
const fs = require("fs");
const { spawnSync } = require("child_process");

const binName = process.platform === "win32" ? "telepager.exe" : "telepager";
const binPath = path.join(__dirname, binName);

if (!fs.existsSync(binPath)) {
  console.error(
    "[telepager] binary not found at " +
      binPath +
      "\n  The install step may have failed. Reinstall, or build from source with\n" +
      "  `cargo build --release` and set TELEPAGER_BINARY."
  );
  process.exit(1);
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error("[telepager] failed to launch: " + result.error.message);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
