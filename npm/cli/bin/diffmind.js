#!/usr/bin/env node
"use strict";

// Thin launcher for the diffmind binary.
//
// The real executable ships in a per-platform package (see optionalDependencies
// in ../package.json). npm installs only the one matching the host's os/cpu, so
// a macOS user never downloads the Windows build. This shim finds it and execs
// it, forwarding argv, stdio and the exit code untouched — `diffmind --tui` and
// the CI gate's exit-1-on-findings both depend on that being transparent.

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const PLATFORMS = {
  "darwin arm64": "@diffmind/cli-darwin-arm64",
  "darwin x64": "@diffmind/cli-darwin-x64",
  "linux x64": "@diffmind/cli-linux-x64",
  "linux arm64": "@diffmind/cli-linux-arm64",
  "win32 x64": "@diffmind/cli-win32-x64",
};

const RELEASES = "https://github.com/thinkgrid-labs/diffmind/releases";
const INSTALL_SH =
  "curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash";

function fail(lines) {
  console.error("\ndiffmind: " + lines.join("\ndiffmind: ") + "\n");
  process.exit(1);
}

// The published Linux binaries are glibc-linked (the release matrix has no musl
// target). Detect musl up front — otherwise exec fails with a bare ENOENT that
// looks like a missing file rather than a missing loader.
function isMusl() {
  if (process.platform !== "linux") return false;
  const report =
    typeof process.report?.getReport === "function"
      ? process.report.getReport()
      : null;
  if (report && report.header && typeof report.header.glibcVersionRuntime === "string") {
    return false;
  }
  // No glibcVersionRuntime in the report means a non-glibc libc.
  return report !== null;
}

function binaryName() {
  return process.platform === "win32" ? "diffmind.exe" : "diffmind";
}

// Resolve the platform package. require.resolve handles npm, pnpm and yarn
// node_modules layouts; the relative fallback covers the case where the shim is
// run straight out of a checkout with the packages laid out side by side.
function findBinary(pkg) {
  const rel = path.join("bin", binaryName());

  try {
    const manifest = require.resolve(pkg + "/package.json", { paths: [__dirname] });
    const candidate = path.join(path.dirname(manifest), rel);
    if (fs.existsSync(candidate)) return candidate;
  } catch {
    // fall through to the sibling lookup
  }

  const sibling = path.join(__dirname, "..", "..", "platform", pkg.split("/")[1], rel);
  if (fs.existsSync(sibling)) return sibling;

  return null;
}

function main() {
  const key = process.platform + " " + process.arch;
  const pkg = PLATFORMS[key];

  if (!pkg) {
    fail([
      `no prebuilt binary for ${process.platform}-${process.arch}.`,
      `Supported: ${Object.keys(PLATFORMS).join(", ")}.`,
      `Build from source instead: cargo install --git https://github.com/thinkgrid-labs/diffmind diffmind`,
    ]);
  }

  if (isMusl()) {
    fail([
      "the published Linux binaries are glibc-linked and will not run on musl (Alpine).",
      "Use a glibc image (e.g. node:22-slim), or build from source:",
      "  cargo install --git https://github.com/thinkgrid-labs/diffmind diffmind",
    ]);
  }

  const binary = findBinary(pkg);
  if (!binary) {
    fail([
      `the platform package ${pkg} is missing.`,
      "This usually means the install ran with --no-optional or --omit=optional.",
      "Reinstall with optional dependencies enabled:",
      `  npm install ${pkg}`,
      "",
      `Or skip npm entirely: ${INSTALL_SH}`,
      `Binaries: ${RELEASES}`,
    ]);
  }

  // chmod is a no-op on a correctly packed tarball, but npm has historically
  // dropped the executable bit in some install paths; cheap to make sure.
  if (process.platform !== "win32") {
    try {
      fs.chmodSync(binary, 0o755);
    } catch {
      // Read-only store (pnpm, Nix). If the bit is already set this is fine.
    }
  }

  const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

  if (result.error) {
    fail([`failed to run ${binary}: ${result.error.message}`]);
  }

  // Re-raise a fatal signal so the parent shell sees the real cause (Ctrl-C in
  // the TUI must not look like a clean exit 0).
  if (result.signal) {
    process.kill(process.pid, result.signal);
    return;
  }

  process.exit(result.status === null ? 1 : result.status);
}

main();
