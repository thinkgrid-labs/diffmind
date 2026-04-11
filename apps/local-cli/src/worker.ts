import { parentPort, workerData } from "worker_threads";
import * as fs from "fs";

/**
 * Diffmind Background Worker
 * 
 * Handles heavy Wasm inference off the main thread to keep the CLI responsive.
 */

async function runWorker() {
  if (!parentPort) return;

  try {
    const { modelPath, tokenizerPath, diff, context, maxTokens } = workerData;

    // 1. Load Wasm bindings
    const { ReviewAnalyzer } = require("@diffmind/core-wasm");

    // 2. Read model files
    const modelBytes = fs.readFileSync(modelPath);
    const tokenizerBytes = fs.readFileSync(tokenizerPath);

    // 3. Initialize and run
    const analyzer = new ReviewAnalyzer(modelBytes, tokenizerBytes);
    const resultJson = analyzer.analyze_diff_chunked(diff, context || "", maxTokens);

    // 4. Return results
    parentPort.postMessage({ success: true, data: resultJson });
  } catch (err) {
    parentPort.postMessage({ 
      success: false, 
      error: err instanceof Error ? err.message : String(err) 
    });
  }
}

runWorker();
