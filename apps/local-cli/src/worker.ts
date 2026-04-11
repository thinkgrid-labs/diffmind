import { parentPort, workerData } from "worker_threads";
import { EngineRouter } from "./engine/router";

/**
 * Diffmind Background Worker
 * 
 * Handles heavy inference off the main thread.
 * SMART ROUTER: Dynamically selects between Native Engine and Wasm Engine.
 */

async function runWorker() {
  if (!parentPort) return;

  try {
    const { 
      modelPath, 
      tokenizerPath, 
      diff, 
      context, 
      maxTokens, 
      modelId 
    } = workerData;
    
    // 1. SELECT ENGINE & INITIALIZE ANALYZER
    const { analyzer, engineType } = await EngineRouter.getAnalyzer(
      modelId,
      modelPath,
      tokenizerPath
    );

    // 2. RUN ANALYSIS
    // Both Native and Wasm now wrapped in a consistent async interface
    const resultJson = await analyzer.analyze(diff, context || "", maxTokens);

    // 3. RETURN RESULTS
    parentPort.postMessage({ 
      success: true, 
      data: resultJson, 
      engine: engineType 
    });
  } catch (err) {
    parentPort.postMessage({ 
      success: false, 
      error: err instanceof Error ? err.message : String(err) 
    });
  }
}

runWorker();
