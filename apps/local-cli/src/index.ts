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
import { Command } from "commander";
import chalk from "chalk";
import ora from "ora";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import * as https from "https";
import * as http from "http";
import * as child_process from "child_process";
import { SingleBar, Presets } from "cli-progress";
import {
  parseReport,
  sortFindings,
  filterBySeverity,
  type ReviewReport,
  type Severity,
  type Category,
} from "@diffmind/shared-types";

// ─── Constants ────────────────────────────────────────────────────────────────

const MODEL_DIR = path.join(os.homedir(), ".diffmind", "models");
const MODEL_FILENAME = "Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf";
const TOKENIZER_FILENAME = "tokenizer.json";

// HuggingFace Hub URLs
const MODEL_URL =
  "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf";
const TOKENIZER_URL =
  "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct/resolve/main/tokenizer.json";

// ─── CLI Definition ───────────────────────────────────────────────────────────

const program = new Command();

program
  .command("index")
  .description("Build a symbol index of the local repository for context-aware reviews")
  .action(async () => {
    await runIndexer();
  });

program
  .name("diffmind")
  .description("Local-first AI code review for your git diffs")
  .version("0.3.0")
  .option("-b, --branch <name>", "Target branch to diff against", "main")
  .option(
    "-f, --format <type>",
    'Output format: "markdown" or "json"',
    "markdown"
  )
  .option("-o, --output <file>", "Write output to a file instead of stdout")
  .option(
    "-c, --context <text|file>",
    "Business context (ticket description, acceptance criteria)"
  )
  .option(
    "--min-severity <level>",
    'Minimum severity to report: "high", "medium", or "low"',
    "low"
  )
  .option("--stdin", "Read git diff from stdin instead of running git diff")
  .option("--no-color", "Disable colored output");

// ─── Entry Point ──────────────────────────────────────────────────────────────

function run() {
  program.parse(process.argv);
  
  // If no subcommand is used, run the default analysis
  if (!program.args.length || program.args[0] !== "index") {
    main().catch((err) => {
      console.error(chalk.red(`Fatal Error: ${err.message}`));
      process.exit(1);
    });
  }
}

// Only run if executed directly
if (require.main === module) {
  run();
}

const opts = program.opts<{
  branch: string;
  format: "markdown" | "json";
  output?: string;
  minSeverity: Severity;
  stdin: boolean;
  color: boolean;
  context?: string;
}>();

// ─── Main Logic ───────────────────────────────────────────────────────────────

import { Worker } from "worker_threads";

async function main(): Promise<void> {
  printBanner();

  // 1. Ensure model files are downloaded
  await ensureModelFiles();

  // 2. Get the git diff
  const diff = await getDiff();
  if (!diff.trim()) {
    console.log(chalk.green("✓ No changes detected. Nothing to analyze."));
    process.exit(0);
  }

  // 3. Prepare business context
  let context = "";
  if (opts.context) {
    if (fs.existsSync(opts.context)) {
      context = fs.readFileSync(opts.context, "utf-8");
    } else {
      context = opts.context;
    }
  }

  // 4. Retrieve architectural context (Local RAG Phase 1)
  const ragContext = await getRagContext(diff);
  if (ragContext) {
    context = `${context}\n\n### Architectural Reference (Local RAG):\n${ragContext}`;
  }

  // 5. Run analysis in a background worker
  // This keeps the process responsive and the spinner animated.
  const analyzeSpinner = ora("Initializing engine & analyzing diff...").start();
  
  try {
    const reportRaw = await runAnalysisInWorker({
      modelPath: path.join(MODEL_DIR, MODEL_FILENAME),
      tokenizerPath: path.join(MODEL_DIR, TOKENIZER_FILENAME),
      diff,
      context,
      maxTokens: 2048,
    });

    report = sortFindings(
      filterBySeverity(parseReport(reportRaw), opts.minSeverity)
    );
    analyzeSpinner.succeed(`Analysis complete — ${report.length} finding(s)`);
  } catch (err) {
    analyzeSpinner.fail("Analysis failed");
    console.error(chalk.red(String(err)));
    process.exit(1);
  }

  // 6. Format and output results
  const output =
    opts.format === "json"
      ? formatJson(report)
      : formatMarkdown(report, opts.branch);

  if (opts.output) {
    fs.writeFileSync(opts.output, output, "utf-8");
    console.log(chalk.green(`✓ Report saved to ${opts.output}`));
  } else {
    console.log("\n" + output);
  }

  // Exit code 1 if any HIGH severity findings (useful for CI)
  const hasHigh = report.some((f) => f.severity === "high");
  process.exit(hasHigh ? 1 : 0);
}

let report: ReviewReport;

function runAnalysisInWorker(workerData: {
  modelPath: string;
  tokenizerPath: string;
  diff: string;
  context: string;
  maxTokens: number;
}): Promise<string> {
  return new Promise((resolve, reject) => {
    // Determine the worker path. During development with ts-node, we point to the .ts file
    // In production, we point to the transpiled .js file.
    const isTsNode = process.argv.some(arg => arg.includes('ts-node'));
    const workerPath = isTsNode 
      ? path.join(__dirname, "worker.ts")
      : path.join(__dirname, "worker.js");

    const worker = new Worker(workerPath, {
      workerData,
      // If running via ts-node, we need to register it for the worker thread too
      execArgv: isTsNode ? ["-r", "ts-node/register"] : [],
    });

    worker.on("message", (message) => {
      if (message.success) {
        resolve(message.data);
      } else {
        reject(new Error(message.error));
      }
    });

    worker.on("error", reject);
    worker.on("exit", (code) => {
      if (code !== 0) {
        reject(new Error(`Worker stopped with exit code ${code}`));
      }
    });
  });
}

// ─── Local RAG ────────────────────────────────────────────────────────────────

async function runIndexer(): Promise<void> {
  const spinner = ora("Scanning repository for symbols...").start();
  try {
    const indexer = new Indexer(process.cwd());
    const index = await indexer.buildIndex();
    indexer.save(index);
    spinner.succeed(`Index built: ${Object.keys(index.symbols).length} symbols found`);
  } catch (err) {
    spinner.fail("Indexing failed");
    console.error(chalk.red(String(err)));
    process.exit(1);
  }
}

async function getRagContext(diff: string): Promise<string | null> {
  const index = Indexer.load(process.cwd());
  if (!index) return null;

  const foundSymbols = new Set<string>();
  const lines = diff.split("\n");

  // Simple heuristic: find all words that match a known symbol name in added/modified lines
  for (const line of lines) {
    if (!line.startsWith("+") || line.startsWith("+++")) continue;

    const words = line.match(/[a-zA-Z0-9_$]+/g);
    if (!words) continue;

    for (const word of words) {
      if (index.symbols[word]) {
        foundSymbols.add(word);
      }
    }
  }

  if (foundSymbols.size === 0) return null;

  let contexts = "";
  // Pick top 10 symbols to avoid prompt overflow
  const symbolsToInclude = Array.from(foundSymbols).slice(0, 10);
  
  for (const symName of symbolsToInclude) {
    const def = index.symbols[symName];
    contexts += `\n--- Symbol: ${symName} (from ${def.file}) ---\n${def.snippet}\n`;
  }

  return contexts;
}

// ─── Diff Acquisition ─────────────────────────────────────────────────────────

async function getDiff(): Promise<string> {
  if (opts.stdin) {
    return readStdin();
  }
  const spinner = ora(`Running git diff ${opts.branch}...HEAD`).start();
  try {
    const result = child_process.spawnSync(
      "git",
      ["diff", `${opts.branch}...HEAD`],
      {
        maxBuffer: 10 * 1024 * 1024, // 10MB max diff
        encoding: "utf-8",
        shell: false, // Explicitly disable shell for security
      }
    );

    if (result.status !== 0) {
      throw new Error(result.stderr?.toString() || "Unknown git error");
    }

    const diff = result.stdout.toString();
    spinner.succeed(`Diff captured (${Math.round(diff.length / 1024)}KB)`);
    return diff;
  } catch (err) {
    spinner.fail("Failed to get git diff");
    const msg = err instanceof Error ? err.message : String(err);
    if (msg.includes("not a git repository")) {
      console.error(
        chalk.red(
          "Error: not a git repository. Run diffmind from within a git project."
        )
      );
    } else if (msg.includes("unknown revision")) {
      console.error(
        chalk.red(
          `Error: branch "${opts.branch}" not found. Try a different --branch value.`
        )
      );
    } else {
      console.error(chalk.red(msg));
    }
    process.exit(1);
  }
}

function readStdin(): Promise<string> {
  return new Promise((resolve) => {
    let data = "";
    process.stdin.setEncoding("utf-8");
    process.stdin.on("data", (chunk) => (data += chunk));
    process.stdin.on("end", () => resolve(data));
  });
}

// ─── Model Management ─────────────────────────────────────────────────────────

async function ensureModelFiles(): Promise<void> {
  fs.mkdirSync(MODEL_DIR, { recursive: true });

  const modelPath = path.join(MODEL_DIR, MODEL_FILENAME);
  const tokenizerPath = path.join(MODEL_DIR, TOKENIZER_FILENAME);

  if (!fs.existsSync(tokenizerPath)) {
    console.log(chalk.cyan("Downloading tokenizer.json..."));
    await downloadFile(TOKENIZER_URL, tokenizerPath);
  }

  if (!fs.existsSync(modelPath)) {
    console.log(
      chalk.cyan(
        `\nDownloading ${MODEL_FILENAME} (~2.2GB). This only happens once.\n`
      )
    );
    await downloadFileWithProgress(MODEL_URL, modelPath);
  }
}

function downloadFile(url: string, dest: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    const get = url.startsWith("https") ? https.get : http.get;
    get(url, { headers: { "User-Agent": "diffmind/0.1.0" } }, (res) => {
      if (res.statusCode === 302 || res.statusCode === 301) {
        file.close();
        fs.unlinkSync(dest);
        downloadFile(res.headers.location!, dest).then(resolve).catch(reject);
        return;
      }
      res.pipe(file);
      file.on("finish", () => file.close(() => resolve()));
    }).on("error", (err) => {
      fs.unlinkSync(dest);
      reject(err);
    });
  });
}

function downloadFileWithProgress(url: string, dest: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const get = url.startsWith("https") ? https.get : http.get;
    get(url, { headers: { "User-Agent": "diffmind/0.1.0" } }, (res) => {
      if (res.statusCode === 302 || res.statusCode === 301) {
        downloadFileWithProgress(res.headers.location!, dest)
          .then(resolve)
          .catch(reject);
        return;
      }

      const total = parseInt(res.headers["content-length"] ?? "0", 10);
      const bar = new SingleBar(
        {
          format: `{bar} {percentage}% | {value}/{total} MB | ETA: {eta}s`,
          formatValue: (v, _, type) => {
            if (type === "value" || type === "total")
              return (v / 1024 / 1024).toFixed(1);
            return String(v);
          },
        },
        Presets.shades_classic
      );
      bar.start(total, 0);

      let downloaded = 0;
      const file = fs.createWriteStream(dest);

      res.on("data", (chunk: Buffer) => {
        downloaded += chunk.length;
        bar.update(downloaded);
        file.write(chunk);
      });

      res.on("end", () => {
        bar.stop();
        file.close(() => resolve());
      });

      res.on("error", (err) => {
        bar.stop();
        fs.unlinkSync(dest);
        reject(err);
      });
    }).on("error", reject);
  });
}

// ─── Formatters ───────────────────────────────────────────────────────────────

export function formatJson(report: ReviewReport): string {
  return JSON.stringify(report, null, 2);
}

export function formatMarkdown(report: ReviewReport, branch: string): string {
  const lines: string[] = [];

  if (report.length === 0) {
    lines.push(chalk.green("✓ No issues found in this diff.\n"));
    return lines.join("\n");
  }

  const high = report.filter((f) => f.severity === "high");
  const medium = report.filter((f) => f.severity === "medium");
  const low = report.filter((f) => f.severity === "low");

  lines.push(chalk.bold.white("╔══════════════════════════════════════╗"));
  lines.push(chalk.bold.white("║      diffmind Code Review Report     ║"));
  lines.push(chalk.bold.white("╚══════════════════════════════════════╝"));
  lines.push("");
  lines.push(`Branch: ${chalk.cyan(branch)}  |  Findings: ${chalk.white(report.length)}  |  ${chalk.red(`High: ${high.length}`)}  ${chalk.yellow(`Medium: ${medium.length}`)}  ${chalk.blue(`Low: ${low.length}`)}`);
  lines.push("");

  for (const finding of report) {
    const sBadge = severityBadge(finding.severity);
    const cBadge = categoryBadge(finding.category);
    const conf = finding.confidence != null
      ? chalk.dim(` (confidence: ${Math.round(finding.confidence * 100)}%)`)
      : "";
    lines.push(`${sBadge} ${cBadge} ${chalk.bold(finding.file)}:${chalk.cyan(String(finding.line))}${conf}`);
    lines.push(`  ${chalk.white(finding.issue)}`);
    lines.push(`  ${chalk.dim("Fix:")} ${chalk.green(finding.suggested_fix)}`);
    lines.push("");
  }

  return lines.join("\n");
}

export function severityBadge(severity: Severity): string {
  switch (severity) {
    case "high":   return chalk.bgRed.white.bold(" HIGH ");
    case "medium": return chalk.bgYellow.black.bold(" MED  ");
    case "low":    return chalk.bgBlue.white.bold(" LOW  ");
  }
}

export function categoryBadge(category: Category): string {
  switch (category) {
    case "security":        return chalk.bgMagenta.white.bold(" SECURITY ");
    case "quality":         return chalk.bgCyan.black.bold(" QUALITY  ");
    case "performance":     return chalk.bgBlackBright.white.bold(" PERF     ");
    case "maintainability": return chalk.bgBlueBright.white.bold(" MAINT    ");
  }
}

function printBanner(): void {
  console.log(chalk.cyan.bold("\n  diffmind") + chalk.dim(" v0.3.0 — local-first AI code review"));
  console.log(chalk.dim("  Model: Qwen2.5-Coder-3B-Instruct Q4_K_M | Inference: on-device Wasm\n"));
}

if (require.main === module) {
  main().catch((err) => {
    console.error(chalk.red("\nUnexpected error:"), err);
    process.exit(1);
  });
}
