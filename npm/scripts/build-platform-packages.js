#!/usr/bin/env node
"use strict";

// Turn the built binaries in dist/ into one npm package per platform.
//
// These are what `telepager`'s optionalDependencies point at. npm installs
// only the one matching the machine, and because it's a real dependency rather
// than a postinstall download, `--ignore-scripts` installs work too.
//
//   node npm/scripts/build-platform-packages.js <dist dir> <out dir>

const fs = require("fs");
const path = require("path");

const root = path.resolve(__dirname, "..");
const pkg = require(path.join(root, "package.json"));

const distDir = path.resolve(process.argv[2] || "dist");
const outDir = path.resolve(process.argv[3] || path.join(root, "platforms"));

// keep this in step with TARGETS in install.js
const PLATFORMS = [
  { os: "darwin", cpu: "arm64", triple: "aarch64-apple-darwin" },
  { os: "darwin", cpu: "x64", triple: "x86_64-apple-darwin" },
  { os: "linux", cpu: "x64", triple: "x86_64-unknown-linux-gnu" },
  { os: "linux", cpu: "arm64", triple: "aarch64-unknown-linux-gnu" },
  { os: "win32", cpu: "x64", triple: "x86_64-pc-windows-msvc" },
];

let built = 0;
const missing = [];

for (const p of PLATFORMS) {
  const isWindows = p.os === "win32";
  const ext = isWindows ? ".exe" : "";
  const source = path.join(distDir, "telepager-" + p.triple + ext);

  if (!fs.existsSync(source)) {
    missing.push(path.basename(source));
    continue;
  }

  const name = "telepager-" + p.os + "-" + p.cpu;
  const dir = path.join(outDir, name);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });

  const target = path.join(dir, "bin", "telepager" + ext);
  fs.copyFileSync(source, target);
  if (!isWindows) fs.chmodSync(target, 0o755);

  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify(
      {
        name,
        version: pkg.version,
        description: "telepager binary for " + p.os + " " + p.cpu,
        license: pkg.license,
        repository: pkg.repository,
        homepage: pkg.homepage,
        os: [p.os],
        cpu: [p.cpu],
        // the binary is the whole package
        files: ["bin/"],
        preferUnplugged: true,
      },
      null,
      2
    ) + "\n"
  );

  fs.writeFileSync(
    path.join(dir, "README.md"),
    "# " + name + "\n\nThe telepager binary for " + p.os + " " + p.cpu + ".\n\n" +
      "You don't install this yourself — [`telepager`](https://www.npmjs.com/package/telepager) " +
      "depends on it and npm picks the right one for your machine.\n"
  );

  console.log("packaged " + name + " (" + (fs.statSync(target).size / 1e6).toFixed(1) + " MB)");
  built++;
}

if (missing.length) {
  console.warn("no binary for: " + missing.join(", "));
}
if (built === 0) {
  console.error("nothing to package — is " + distDir + " right?");
  process.exit(1);
}
console.log("built " + built + " platform package(s) in " + outDir);
