#!/usr/bin/env node
"use strict";

// Find the binary and hand the process straight over to it. stdio has to pass
// through untouched: for `telepager mcp`, stdin and stdout are the transport.

const { spawnSync } = require("child_process");
const { resolveBinary, MISSING_MESSAGE } = require("../install.js");

const binary = resolveBinary();
if (!binary) {
  console.error(MISSING_MESSAGE);
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error("[telepager] failed to launch: " + result.error.message);
  process.exit(1);
}
// a child killed by a signal reports null, which isn't a useful exit code
process.exit(result.status === null ? 1 : result.status);
