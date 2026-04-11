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
} from "@diffmind/shared-types";

// ─── Constants ────────────────────────────────────────────────────────────────

const MODEL_DIR = path.join(os.homedir(), ".diffmind", "models");
const MODEL_FILENAME = "qwen2.5-coder-3b-instruct-q4_k_m.gguf";
const TOKENIZER_FILENAME = "tokenizer.json";

// HuggingFace Hub URLs
const MODEL_URL =
  "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/qwen2.5-coder-3b-instruct-q4_k_m.gguf";
const TOKENIZER_URL =
  "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct/resolve/main/tokenizer.json";

// ─── CLI Definition ───────────────────────────────────────────────────────────

const program = new Command();
const opts: {
  branch: string;
  format: "markdown" | "json";
  output?: string;
  minSeverity: Severity;
  stdin: boolean;
  color: boolean;
  context?: string;
} = {} as any;

program
  .name("diffmind")
  .description("Local-first AI code review for your git diffs")
  .version("0.3.9")
  .option("-b, --branch <name>", "Target branch to diff against", "main")
  .option("-f, --format <type>", 'Output format: "markdown" or "json"', "markdown")
  .option("-o, --output <file>", "Write output to a file instead of stdout")
  .option("-c, --context <text|file>", "Business context (ticket description, acceptance criteria)")
  .option("--min-severity <level>", 'Minimum severity to report: "high", "medium", or "low"', "low")
  .option("--stdin", "Read git diff from stdin instead of running git diff")
  .option("--no-color", "Disable colored output")
  .action(async (options) => {
    // Check if we are actually running a subcommand
    // commander names the subcommand as the first element of program.args
    const isSubcommand = program.commands.some(
      (cmd) => program.args[0] === cmd.name() || cmd.aliases().includes(program.args[0])
    );

    if (isSubcommand) {
      return; // Let the subcommand handler take over
    }

    Object.assign(opts, options);
    await main().catch((err) => {
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
  .option("-f, --force", "Force a fresh download of the model and tokenizer")
  .action(async (options) => {
    if (options.force) {
      const modelPath = path.join(MODEL_DIR, MODEL_FILENAME);
      const tokenizerPath = path.join(MODEL_DIR, TOKENIZER_FILENAME);
      if (fs.existsSync(modelPath)) fs.unlinkSync(modelPath);
      if (fs.existsSync(tokenizerPath)) fs.unlinkSync(tokenizerPath);
      console.log(chalk.yellow("✓ Force flag active: existing model files cleared."));
    }
    await ensureModelFiles();
    console.log(chalk.green("\n✓ Setup complete. Model is ready for use."));
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

  if (fs.existsSync(modelPath) && fs.statSync(modelPath).size < 1024) {
    fs.unlinkSync(modelPath);
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
      const isRedirect = [301, 302, 307, 308].includes(res.statusCode || 0);
      if (isRedirect && res.headers.location) {
        file.close();
        fs.unlinkSync(dest);
        const nextUrl = new URL(res.headers.location, url).href;
        downloadFile(nextUrl, dest).then(resolve).catch(reject);
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
      const isRedirect = [301, 302, 307, 308].includes(res.statusCode || 0);
      if (isRedirect && res.headers.location) {
        const nextUrl = new URL(res.headers.location, url).href;
        console.log(chalk.dim(`  Redirecting to: ${nextUrl}`));
        downloadFileWithProgress(nextUrl, dest)
          .then(resolve)
          .catch(reject);
        return;
      }

      const statusCode = res.statusCode || 0;
      if (statusCode < 200 || statusCode >= 300) {
        reject(new Error(`Server returned status code ${statusCode} for ${url}`));
        return;
      }

      const contentLength = res.headers["content-length"];
      const total = contentLength ? parseInt(contentLength, 10) : 0;
      
      const bar = new SingleBar(
        {
          format: `{bar} {percentage}% | {value}${total ? "/{total}" : ""} MB | ETA: {eta}s`,
          formatValue: (v, _, type) => {
            if (type === "value" || type === "total")
              return (v / 1024 / 1024).toFixed(1);
            return String(v);
          },
        },
        Presets.shades_classic
      );
      
      if (total > 0) {
        bar.start(total, 0);
      } else {
        console.log(chalk.dim("  (Total size unknown, streaming...)"));
        bar.start(1, 0); // Placeholder start
      }

      let downloaded = 0;
      const file = fs.createWriteStream(dest);

      res.on("data", (chunk: Buffer) => {
        downloaded += chunk.length;
        if (total > 0) {
          bar.update(downloaded);
        } else {
          // If unknown, just show current progress in MB
          bar.update(1, { value: downloaded }); 
        }
        file.write(chunk);
      });

      res.on("end", () => {
        bar.stop();
        file.close(() => {
          const isModel = dest.endsWith(".gguf");
          const minSize = isModel ? 1024 * 1024 * 100 : 1024; // 100MB for model, 1kb for tokenizer
          
          if (downloaded < minSize) {
            fs.unlinkSync(dest);
            reject(
              new Error(
                `Download failed: file is too small (${(
                  downloaded / 1024 / 1024
                ).toFixed(2)} MB received). The connection may have been throttled or interrupted.`
              )
            );
          } else {
            console.log(
              chalk.dim(
                `  Downloaded: ${(downloaded / 1024 / 1024).toFixed(1)} MB`
              )
            );
            resolve();
          }
        });
      });

      res.on("error", (err) => {
        bar.stop();
        fs.unlinkSync(dest);
        reject(err);
      });
    }).on("error", reject);
  });
}

// End of file
