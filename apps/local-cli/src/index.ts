#!/usr/bin/env node
/**
 * diffmind CLI
 *
 * Local-first AI code review for your git diffs.
 * Powered by Qwen2.5-Coder-3B running entirely on-device via WebAssembly.
 *
 *   git diff main...HEAD | diffmind --stdin
 */

import { Indexer } from "./indexer";
import { getRagContext } from "./rag";
import {
  formatJson,
  formatMarkdown,
  printBanner,
} from "./formatters";
import { Command } from "commander";
import chalk from "chalk";
import ora from "ora";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import * as https from "node:https";
import * as http from "node:http";
import * as child_process from "node:child_process";
import { SingleBar, Presets } from "cli-progress";
import {
  parseReport,
  sortFindings,
  filterBySeverity,
  type ReviewReport,
  type Severity,
} from "@diffmind/shared-types";

// ─── Constants ────────────────────────────────────────────────────────────────

const MODEL_DIR = path.join(os.homedir(), ".diffmind", "models");
const TOKENIZER_FILENAME = "tokenizer.json";

interface ModelConfig {
  id: string;
  name: string;
  filename: string;
  modelUrl: string;
  tokenizerUrl: string;
  minMemoryGB: number;
}

const MODELS: Record<string, ModelConfig> = {
  "1.5b": {
    id: "1.5b",
    name: "Qwen2.5-Coder-1.5B-Instruct Q4_K_M",
    filename: "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
    modelUrl: "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/resolve/main/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
    tokenizerUrl: "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct/resolve/main/tokenizer.json",
    minMemoryGB: 2,
  },
  "3b": {
    id: "3b",
    name: "Qwen2.5-Coder-3B-Instruct Q4_K_M",
    filename: "qwen2.5-coder-3b-instruct-q4_k_m.gguf",
    // Use the official Qwen GGUF (same source as 1.5B). The bartowski GGUF
    // stores rope metadata in a format candle 0.8 doesn't parse correctly,
    // resulting in a zero-length RoPE cache and a panic during inference.
    modelUrl: "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/qwen2.5-coder-3b-instruct-q4_k_m.gguf",
    tokenizerUrl: "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct/resolve/main/tokenizer.json",
    minMemoryGB: 4,
  }
};

const DEFAULT_MODEL = "1.5b";

/**
 * Common generated or dependency files that should be excluded from the AI review
 * to prevent massive diffs and memory exhaustion.
 */
const DEFAULT_IGNORED_PATHSPECS = [
  ":!node_modules",
  ":!*-lock.json",
  ":!pnpm-lock.yaml",
  ":!package-lock.json",
  ":!yarn.lock",
  ":!dist",
  ":!build",
  ":!.next",
  ":!.cache",
  ":!*.map",
  ":!*.min.js",
  ":!*.min.css",
];

interface Config {
  model?: string;
}

const CONFIG_PATH = path.join(os.homedir(), ".diffmind", "config.json");

function readConfig(): Config {
  try {
    return JSON.parse(fs.readFileSync(CONFIG_PATH, "utf-8")) as Config;
  } catch {
    return {};
  }
}

/** Writes config atomically (temp file → rename) to avoid torn writes in parallel runs. */
function writeConfig(config: Config): void {
  const tmp = CONFIG_PATH + ".tmp";
  fs.writeFileSync(tmp, JSON.stringify(config, null, 2), "utf-8");
  fs.renameSync(tmp, CONFIG_PATH);
}

function getActiveModelId(): string {
  return readConfig().model ?? DEFAULT_MODEL;
}

function setActiveModelId(id: string): void {
  const configDir = path.join(os.homedir(), ".diffmind");
  fs.mkdirSync(configDir, { recursive: true });
  const config = readConfig();
  config.model = id;
  writeConfig(config);
}

// ─── CLI Definition ───────────────────────────────────────────────────────────

const program = new Command();
const opts: {
  branch: string;
  format: "markdown" | "json";
  output?: string;
  minSeverity: Severity;
  maxTokens: number;
  stdin: boolean;
  color: boolean;
  context?: string;
} = {
  branch: "main",
  format: "markdown",
  minSeverity: "low",
  maxTokens: 1024,
  stdin: false,
  color: true,
};

program
  .name("diffmind")
  .description("Local-first AI code review for your git diffs")
  .version("0.4.8")
  .argument("[files...]", "Specific files or directories to review (optional)")
  .option("-b, --branch <name>", "Target branch to diff against", "main")
  .option("-f, --format <type>", 'Output format: "markdown" or "json"', "markdown")
  .option("-o, --output <file>", "Write output to a file instead of stdout")
  .option("-c, --context <text|file>", "Business context (ticket description, acceptance criteria)")
  .option("--min-severity <level>", 'Minimum severity to report: "high", "medium", or "low"', "low")
  .option("--max-tokens <n>", "Maximum output tokens per diff chunk", (v) => Number.parseInt(v, 10), 1024)
  .option("--stdin", "Read git diff from stdin instead of running git diff")
  .option("--no-color", "Disable colored output")
  .action(async (files, options) => {
    // Check if we are actually running a subcommand
    // commander names the subcommand as the first element of program.args
    const isSubcommand = program.commands.some(
      (cmd) => files[0] === cmd.name() || cmd.aliases().includes(files[0])
    );

    if (isSubcommand) {
      return; // Let the subcommand handler take over
    }

    Object.assign(opts, options);
    await main(files).catch((err) => {
      console.error(chalk.red(`Fatal Error: ${err.message}`));
      process.exit(1);
    });
  });

program
  .command("index")
  .description("Build a symbol index of the local repository for context-aware reviews")
  .action(async () => {
    await runIndexer();
  });

program
  .command("download")
  .description("Download or refresh the local AI model files")
  .option("-m, --model <type>", "Model size to download (1.5b, 3b)", DEFAULT_MODEL)
  .option("-f, --force", "Force a fresh download of the model and tokenizer")
  .action(async (options) => {
    const modelId = options.model.toLowerCase();
    if (!MODELS[modelId]) {
      console.error(chalk.red(`Error: Invalid model "${options.model}". Available: 1.5b, 3b`));
      process.exit(1);
    }
    
    setActiveModelId(modelId);
    const model = MODELS[modelId];

    if (options.force) {
      const modelPath = path.join(MODEL_DIR, model.filename);
      const tokenizerPath = path.join(MODEL_DIR, TOKENIZER_FILENAME);
      if (fs.existsSync(modelPath)) fs.unlinkSync(modelPath);
      if (fs.existsSync(tokenizerPath)) fs.unlinkSync(tokenizerPath);
      console.log(chalk.yellow(`✓ Force flag active: existing ${model.id} model files cleared.`));
    }
    
    await ensureModelFiles(modelId);
    console.log(chalk.green(`\n✓ Setup complete. Model ${model.name} is ready for use.`));
  });

// ─── Entry Point ──────────────────────────────────────────────────────────────

function run() {
  program.parse(process.argv);
}

// Only run if executed directly
if (require.main === module) {
  run();
}

// ─── Main Logic ───────────────────────────────────────────────────────────────

import { Worker } from "node:worker_threads";

/** Resolves business context from --context flag and appends any RAG snippets. */
async function buildContext(diff: string): Promise<string> {
  let context = "";
  if (opts.context) {
    context = fs.existsSync(opts.context)
      ? fs.readFileSync(opts.context, "utf-8")
      : opts.context;
  }
  const ragContext = await getRagContext(diff);
  if (ragContext) {
    context = `${context}\n\n### Architectural Reference (Local RAG):\n${ragContext}`;
  }
  return context;
}

/** Runs the worker with a progress spinner and returns the filtered report. */
async function runAnalysisWithSpinner(
  model: ModelConfig,
  diff: string,
  context: string,
): Promise<ReviewReport> {
  const spinner = ora("Initializing engine & analyzing diff...").start();
  let pulseCount = 0;
  const pulseInterval = setInterval(() => {
    pulseCount++;
    const dots = ".".repeat((pulseCount % 3) + 1);
    spinner.text = `Analyzing diff (Local AI is thinking${dots})`;
  }, 3000);

  try {
    // Forward worker progress messages to the spinner so the user sees
    // "Loading model…" vs "Running inference…" instead of a frozen cursor.
    const { data: reportRaw, engine } = await runAnalysisInWorker(
      {
        modelPath: path.join(MODEL_DIR, model.filename),
        tokenizerPath: path.join(MODEL_DIR, TOKENIZER_FILENAME),
        diff,
        context,
        maxTokens: opts.maxTokens,
        modelId: model.id,
      },
      (text) => { spinner.text = text; },
    );
    const findings = sortFindings(filterBySeverity(parseReport(reportRaw), opts.minSeverity));
    spinner.succeed(`Analysis complete [engine: ${engine}] — ${findings.length} finding(s)`);
    return findings;
  } catch (err) {
    spinner.fail("Analysis failed");
    console.error(chalk.red(String(err)));
    process.exit(1);
  } finally {
    clearInterval(pulseInterval);
  }
}

/** Writes the formatted report to a file or stdout. */
function printOutput(output: string): void {
  if (opts.output) {
    fs.writeFileSync(opts.output, output, "utf-8");
    console.log(chalk.green(`✓ Report saved to ${opts.output}`));
  } else {
    console.log("\n" + output);
  }
}

async function main(files: string[] = []): Promise<void> {
  const modelId = getActiveModelId();
  const model = MODELS[modelId];
  printBanner();
  console.log(chalk.dim(`  Using Model: ${model.name}`));

  await ensureModelFiles(modelId);

  const diff = await getDiff(files);
  if (!diff.trim()) {
    console.log(chalk.green("✓ No changes detected. Nothing to analyze."));
    process.exit(0);
  }

  const context = await buildContext(diff);
  report = await runAnalysisWithSpinner(model, diff, context);

  const output = opts.format === "json"
    ? formatJson(report)
    : formatMarkdown(report, opts.branch);

  printOutput(output);
  process.exit(report.some((f) => f.severity === "high") ? 1 : 0);
}

let report: ReviewReport;

// Kill the worker and reject if inference stalls (OOM, deadlock, etc.).
const WORKER_TIMEOUT_MS = 10 * 60 * 1000; // 10 minutes

function runAnalysisInWorker(
  workerData: {
    modelPath: string;
    tokenizerPath: string;
    diff: string;
    context: string;
    maxTokens: number;
    modelId: string;
  },
  onProgress?: (text: string) => void,
): Promise<{ data: string; engine: string }> {
  return new Promise((resolve, reject) => {
    const isTsNode = process.argv.some(arg => arg.includes('ts-node'));
    const workerPath = isTsNode
      ? path.join(__dirname, "worker.ts")
      : path.join(__dirname, "worker.js");

    const worker = new Worker(workerPath, {
      workerData,
      execArgv: isTsNode ? ["-r", "ts-node/register"] : [],
    });

    let settled = false;
    const settle = (fn: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      fn();
    };

    const timer = setTimeout(() => {
      worker.terminate();
      settle(() =>
        reject(new Error(`Analysis timed out after ${WORKER_TIMEOUT_MS / 60000} minutes`))
      );
    }, WORKER_TIMEOUT_MS);

    worker.on("message", (message) => {
      if (message.type === "progress") {
        onProgress?.(message.text);
        return;
      }
      settle(() => {
        if (message.success) {
          resolve({ data: message.data, engine: message.engine });
        } else {
          reject(new Error(message.error));
        }
      });
    });

    worker.on("error", (err) => settle(() => reject(err)));
    worker.on("exit", (code) => {
      settle(() => {
        if (code !== 0) {
          reject(new Error(`Worker stopped with exit code ${code}`));
        }
      });
    });
  });
}

// ─── Local RAG ────────────────────────────────────────────────────────────────

async function runIndexer(): Promise<void> {
  const spinner = ora("Scanning repository for symbols...").start();
  try {
    const cwd = process.cwd();
    const existingIndex = Indexer.load(cwd);
    const indexer = new Indexer(cwd);
    
    const index = await indexer.buildIndex(existingIndex);
    indexer.save(index);
    spinner.succeed(`Index updated: ${Object.keys(index.symbols).length} symbols found`);
  } catch (err) {
    spinner.fail("Indexing failed");
    console.error(chalk.red(String(err)));
    process.exit(1);
  }
}

// ─── Diff Acquisition ─────────────────────────────────────────────────────────

// Regex captures the b/ path which may contain spaces, up to end of line.
// git always formats this header as: diff --git a/<path> b/<path>
const DIFF_HEADER_RE = /^diff --git a\/.+ b\/(.+)$/;

function extractFilesFromDiff(diff: string): string[] {
  const files = new Set<string>();
  for (const line of diff.split("\n")) {
    const match = DIFF_HEADER_RE.exec(line);
    if (match) files.add(match[1]);
  }
  return Array.from(files);
}

function logDiffFiles(diff: string): void {
  const capturedFiles = extractFilesFromDiff(diff);
  if (capturedFiles.length > 0) {
    const label = capturedFiles.length <= 12
      ? capturedFiles.join(", ")
      : `${capturedFiles.length} files detected`;
    console.log(chalk.dim(`  Files: ${label}`));
  } else if (diff.trim().length > 0) {
    console.log(chalk.dim("  Files: Detected changes in diff content"));
  }
}

function warnLargeDiff(sizeKB: number): void {
  if (sizeKB > 500) {
    console.log(chalk.yellow(`\n⚠️  Warning: Large diff detected (${sizeKB}KB).`));
    console.log(chalk.dim("  Analyzing very large diffs can be slow on local AI and may impact quality."));
    console.log(chalk.dim("  Consider reviewing in smaller increments or using --branch to target specific changes.\n"));
  }
}

function printGitDiffError(msg: string): never {
  if (msg.includes("not a git repository")) {
    console.error(chalk.red("Error: not a git repository. Run diffmind from within a git project."));
  } else if (msg.includes("unknown revision")) {
    console.error(chalk.red(`Error: branch "${opts.branch}" not found. Try a different --branch value.`));
  } else {
    console.error(chalk.red(msg));
  }
  process.exit(1);
}

async function getDiff(includePaths: string[] = []): Promise<string> {
  if (opts.stdin) {
    return readStdin();
  }

  const targetPaths = includePaths.length > 0 ? includePaths : ["."];
  const spinner = ora(`Running git diff ${opts.branch}...HEAD`).start();
  try {
    const result = child_process.spawnSync(
      "git",
      ["diff", `${opts.branch}...HEAD`, "--", ...targetPaths, ...DEFAULT_IGNORED_PATHSPECS],
      { maxBuffer: 20 * 1024 * 1024, encoding: "utf-8", shell: false },
    );

    if (result.status !== 0) {
      throw new Error(result.stderr?.toString() || "Unknown git error");
    }

    const diff = result.stdout.toString();
    const sizeKB = Math.round(diff.length / 1024);
    spinner.succeed(`Diff captured (${sizeKB}KB)`);
    logDiffFiles(diff);

    if (sizeKB > 1500) {
      spinner.fail(chalk.red(`Diff too large to process (${sizeKB}KB)`));
      console.log(chalk.yellow("\n⚠️  The diff exceeds the 1.5MB safety limit for local AI."));
      console.log(chalk.dim("  This is usually caused by large generated files or many changes at once."));
      console.log(chalk.dim(`  Try reviewing specific files or smaller branches using:`));
      console.log(chalk.cyan(`    diffmind --branch ${opts.branch} path/to/folder\n`));
      process.exit(1);
    }

    warnLargeDiff(sizeKB);
    return diff;
  } catch (err) {
    spinner.fail("Failed to get git diff");
    printGitDiffError(err instanceof Error ? err.message : String(err));
  }
}

const STDIN_SIZE_LIMIT = 1.5 * 1024 * 1024; // 1.5 MB — same cap as git diff path

function readStdin(): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = "";
    let totalBytes = 0;
    process.stdin.setEncoding("utf-8");
    process.stdin.on("data", (chunk: string) => {
      totalBytes += Buffer.byteLength(chunk, "utf-8");
      if (totalBytes > STDIN_SIZE_LIMIT) {
        process.stdin.destroy(
          new Error("stdin input exceeds the 1.5MB safety limit; pipe a smaller diff")
        );
        return;
      }
      data += chunk;
    });
    process.stdin.on("end", () => resolve(data));
    process.stdin.on("error", reject);
  });
}

// ─── Model Management ─────────────────────────────────────────────────────────

async function ensureModelFiles(modelId: string = getActiveModelId()): Promise<void> {
  const model = MODELS[modelId];
  fs.mkdirSync(MODEL_DIR, { recursive: true });

  const modelPath = path.join(MODEL_DIR, model.filename);
  const tokenizerPath = path.join(MODEL_DIR, TOKENIZER_FILENAME);

  if (!fs.existsSync(tokenizerPath)) {
    console.log(chalk.cyan("Downloading tokenizer.json..."));
    await downloadFile(model.tokenizerUrl, tokenizerPath);
  }

  if (fs.existsSync(modelPath) && fs.statSync(modelPath).size < 1024) {
    fs.unlinkSync(modelPath);
  }

  if (!fs.existsSync(modelPath)) {
    console.log(
      chalk.cyan(
        `\nDownloading ${model.filename} (${(model.minMemoryGB * 0.5).toFixed(1)}GB+). This only happens once.\n`
      )
    );
    await downloadFileWithProgress(model.modelUrl, modelPath);
  }
}

// ─── Download Helpers (kept at module scope to stay within 4-level nesting) ───

function deleteAndReject(dest: string, err: Error, reject: (e: Error) => void): void {
  fs.unlink(dest, () => reject(err));
}

function deleteAndContinue(
  dest: string,
  nextUrl: string,
  redirectsLeft: number,
  resolve: () => void,
  reject: (e: Error) => void,
): void {
  fs.unlink(dest, () => {
    downloadFile(nextUrl, dest, redirectsLeft - 1).then(resolve).catch(reject);
  });
}

function finalizeDownload(
  dest: string,
  downloaded: number,
  resolve: () => void,
  reject: (e: Error) => void,
): void {
  const isModel = dest.endsWith(".gguf");
  const minSize = isModel ? 1024 * 1024 * 100 : 1024;
  if (downloaded < minSize) {
    fs.unlink(dest, () =>
      reject(
        new Error(
          `Download failed: file is too small (${(downloaded / 1024 / 1024).toFixed(2)} MB received). The connection may have been throttled or interrupted.`
        )
      )
    );
  } else {
    console.log(chalk.dim(`  Downloaded: ${(downloaded / 1024 / 1024).toFixed(1)} MB`));
    resolve();
  }
}

function closeAndResolve(
  file: fs.WriteStream,
  resolve: () => void,
  reject: (e: Error) => void,
): void {
  file.close((err) => (err ? reject(err) : resolve()));
}

function createProgressBar(total: number): SingleBar {
  const bar = new SingleBar(
    {
      format: `{bar} {percentage}% | {value}${total ? "/{total}" : ""} MB | ETA: {eta}s`,
      formatValue: (v: number, _: unknown, type: string) => {
        if (type === "value" || type === "total") return (v / 1024 / 1024).toFixed(1);
        return String(v);
      },
    },
    Presets.shades_classic,
  );
  if (total > 0) {
    bar.start(total, 0);
  } else {
    console.log(chalk.dim("  (Total size unknown, streaming...)"));
    bar.start(1, 0);
  }
  return bar;
}

function streamResponseToFile(
  res: http.IncomingMessage,
  file: fs.WriteStream,
  bar: SingleBar,
  total: number,
  dest: string,
  resolve: () => void,
  reject: (e: Error) => void,
): void {
  let downloaded = 0;
  let settled = false;

  const cleanupFile = (err: Error) => {
    if (settled) return;
    settled = true;
    bar.stop();
    file.destroy();
    file.once("close", () => deleteAndReject(dest, err, reject));
  };

  file.on("error", cleanupFile);
  res.on("error", cleanupFile);

  res.on("data", (chunk: Buffer) => {
    downloaded += chunk.length;
    if (total > 0) {
      bar.update(downloaded);
    } else {
      bar.update(1, { value: downloaded });
    }
    const canContinue = file.write(chunk);
    if (!canContinue) {
      res.pause();
      file.once("drain", () => res.resume());
    }
  });

  res.on("end", () => {
    if (settled) return;
    bar.stop();
    file.close(() => {
      if (settled) return;
      settled = true;
      finalizeDownload(dest, downloaded, resolve, reject);
    });
  });
}

function handleProgressResponse(
  res: http.IncomingMessage,
  url: string,
  dest: string,
  redirectsLeft: number,
  resolve: () => void,
  reject: (e: Error) => void,
): void {
  const isRedirect = [301, 302, 307, 308].includes(res.statusCode || 0);
  if (isRedirect && res.headers.location) {
    if (redirectsLeft <= 0) {
      reject(new Error(`Too many redirects downloading ${url}`));
      return;
    }
    const nextUrl = new URL(res.headers.location, url).href;
    console.log(chalk.dim(`  Redirecting to: ${nextUrl}`));
    downloadFileWithProgress(nextUrl, dest, redirectsLeft - 1).then(resolve).catch(reject);
    return;
  }

  const statusCode = res.statusCode || 0;
  if (statusCode < 200 || statusCode >= 300) {
    reject(new Error(`Server returned status code ${statusCode} for ${url}`));
    return;
  }

  const total = res.headers["content-length"]
    ? Number.parseInt(res.headers["content-length"], 10)
    : 0;

  const bar = createProgressBar(total);
  const file = fs.createWriteStream(dest);
  streamResponseToFile(res, file, bar, total, dest, resolve, reject);
}

function downloadFile(url: string, dest: string, redirectsLeft = 10): Promise<void> {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    let settled = false;

    // Close the write stream, delete the partial file, then reject.
    // Using destroy() + once("close") ensures the fd is released before
    // fs.unlink — otherwise Windows throws EBUSY.
    const cleanup = (err: Error) => {
      if (settled) return;
      settled = true;
      file.destroy();
      file.once("close", () => deleteAndReject(dest, err, reject));
    };

    file.on("error", cleanup);

    const get = url.startsWith("https") ? https.get : http.get;
    const req = get(url, { headers: { "User-Agent": "diffmind/0.1.0" } }, (res) => {
      const isRedirect = [301, 302, 307, 308].includes(res.statusCode || 0);
      if (isRedirect && res.headers.location) {
        if (redirectsLeft <= 0) {
          cleanup(new Error(`Too many redirects downloading ${url}`));
          return;
        }
        const nextUrl = new URL(res.headers.location, url).href;
        file.destroy();
        file.once("close", () => deleteAndContinue(dest, nextUrl, redirectsLeft, resolve, reject));
        return;
      }
      res.on("error", cleanup);
      res.pipe(file);
      file.on("finish", () => {
        if (!settled) {
          settled = true;
          closeAndResolve(file, resolve, reject);
        }
      });
    });

    req.on("error", cleanup);
  });
}

function downloadFileWithProgress(url: string, dest: string, redirectsLeft = 10): Promise<void> {
  return new Promise((resolve, reject) => {
    const get = url.startsWith("https") ? https.get : http.get;
    const req = get(url, { headers: { "User-Agent": "diffmind/0.1.0" } }, (res) =>
      handleProgressResponse(res, url, dest, redirectsLeft, resolve, reject),
    );
    req.on("error", reject);
  });
}

// End of file
