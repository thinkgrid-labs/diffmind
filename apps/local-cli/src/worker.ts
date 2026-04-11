import { parentPort, workerData } from "node:worker_threads";
import { EngineRouter } from "./engine/router";

/**
 * Diffmind Background Worker
 *
 * Handles heavy inference off the main thread.
 * SMART ROUTER: Dynamically selects between Native Engine and Wasm Engine.
 */

function progress(text: string): void {
  parentPort!.postMessage({ type: "progress", text });
}

async function runWorker() {
  if (!parentPort) return;

  try {
    const { modelPath, tokenizerPath, diff, context, maxTokens, modelId } =
      workerData;

    // 1. SELECT ENGINE & INITIALIZE ANALYZER
    // This step reads ~900 MB from disk and deserializes model weights —
    // notify the spinner so the user knows we're loading, not hung.
    progress("Loading model into memory (first run may take 10–30s)…");
    const { analyzer, engineType } = await EngineRouter.getAnalyzer(
      modelId,
      modelPath,
      tokenizerPath,
    );

    // 2. RUN ANALYSIS
    progress(`Running inference [${engineType}]…`);
    const resultJson = await analyzer.analyze(diff, context || "", maxTokens);

    // 3. RETURN RESULTS
    parentPort.postMessage({
      success: true,
      data: resultJson,
      engine: engineType,
    });
  } catch (err) {
    parentPort.postMessage({
      success: false,
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

runWorker();
