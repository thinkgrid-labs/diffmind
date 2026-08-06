#!/usr/bin/env node
// Prepares the npm packages for publishing.
//
//   node npm/scripts/prepare.mjs --version 0.7.0 --artifacts <dir>
//
// 1. Stamps <version> into @diffmind/cli, its optionalDependencies, and every
//    platform package, so all six publish as one matched set.
// 2. Copies each release binary out of the built artifacts into its platform
//    package's bin/ directory.
//
// The artifacts directory is what the release workflow downloads: one
// `diffmind-<target>.tar.gz` per Unix target and a `.zip` for Windows. Pass
// --skip-binaries to stamp versions only (useful for a dry run).

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const NPM_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

// platform package dir -> release target triple
const TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? null : process.argv[i + 1];
}

const version = (arg("version") || "").replace(/^v/, "");
const artifacts = arg("artifacts");
const skipBinaries = process.argv.includes("--skip-binaries");

if (!/^\d+\.\d+\.\d+/.test(version)) {
  console.error("prepare: --version must be a semver like 0.7.0 (got: " + version + ")");
  process.exit(1);
}
if (!skipBinaries && !artifacts) {
  console.error("prepare: --artifacts <dir> is required unless --skip-binaries is passed");
  process.exit(1);
}

function writeJson(file, data) {
  fs.writeFileSync(file, JSON.stringify(data, null, 2) + "\n");
}

// ─── 1. Stamp versions ───────────────────────────────────────────────────────

const cliManifest = path.join(NPM_DIR, "cli", "package.json");
const cli = JSON.parse(fs.readFileSync(cliManifest, "utf8"));
cli.version = version;
for (const dir of Object.keys(TARGETS)) {
  cli.optionalDependencies[`@diffmind/cli-${dir}`] = version;
}
writeJson(cliManifest, cli);
console.log(`@diffmind/cli -> ${version}`);

for (const dir of Object.keys(TARGETS)) {
  const manifest = path.join(NPM_DIR, "platform", dir, "package.json");
  const pkg = JSON.parse(fs.readFileSync(manifest, "utf8"));
  pkg.version = version;
  writeJson(manifest, pkg);
  console.log(`@diffmind/cli-${dir} -> ${version}`);
}

if (skipBinaries) {
  console.log("\nversions stamped; skipping binaries");
  process.exit(0);
}

// ─── 2. Unpack binaries into the platform packages ───────────────────────────

let placed = 0;

for (const [dir, target] of Object.entries(TARGETS)) {
  const isWindows = dir.startsWith("win32");
  const binName = isWindows ? "diffmind.exe" : "diffmind";
  const archive = path.join(
    artifacts,
    `diffmind-${target}.${isWindows ? "zip" : "tar.gz"}`,
  );

  if (!fs.existsSync(archive)) {
    console.error(`prepare: missing release archive ${archive}`);
    process.exit(1);
  }

  const binDir = path.join(NPM_DIR, "platform", dir, "bin");
  fs.rmSync(binDir, { recursive: true, force: true });
  fs.mkdirSync(binDir, { recursive: true });

  // Each archive holds the binary plus README/LICENSE; extract only the binary.
  if (isWindows) {
    execFileSync("unzip", ["-j", "-o", archive, binName, "-d", binDir], {
      stdio: "inherit",
    });
  } else {
    execFileSync("tar", ["-xzf", archive, "-C", binDir, `./${binName}`], {
      stdio: "inherit",
    });
  }

  const binary = path.join(binDir, binName);
  if (!fs.existsSync(binary)) {
    console.error(`prepare: ${binName} not found in ${archive}`);
    process.exit(1);
  }
  // npm preserves the executable bit from the packed tarball.
  if (!isWindows) fs.chmodSync(binary, 0o755);

  const mb = (fs.statSync(binary).size / 1024 / 1024).toFixed(1);
  console.log(`@diffmind/cli-${dir}: bin/${binName} (${mb} MB)`);
  placed++;
}

if (placed !== Object.keys(TARGETS).length) {
  console.error(`prepare: expected ${Object.keys(TARGETS).length} binaries, placed ${placed}`);
  process.exit(1);
}

console.log(`\nready to publish ${placed + 1} packages at ${version}`);
