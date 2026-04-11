import * as fs from "fs";

export interface Analyzer {
  analyze(diff: string, context: string, maxTokens: number): Promise<string>;
}

export type EngineType = "native" | "wasm";

export class EngineRouter {
  static async getAnalyzer(
    modelId: string,
    modelPath: string,
    tokenizerPath: string
  ): Promise<{ analyzer: Analyzer; engineType: EngineType }> {
    const is3B = modelId === "3b";

    // 1. TRY NATIVE ENGINE FIRST
    try {
      const coreNative = require("@diffmind/core-native");
      const modelBytes = fs.readFileSync(modelPath);
      const tokenizerBytes = fs.readFileSync(tokenizerPath);
      
      const nativeAnalyzer = new coreNative.ReviewAnalyzer(modelBytes, tokenizerBytes);
      
      const analyzer: Analyzer = {
        analyze: async (diff, context, maxTokens) => {
          return await nativeAnalyzer.analyzeDiffChunked(diff, context, maxTokens);
        }
      };

      return { analyzer, engineType: "native" };
    } catch (nativeErr) {
      if (is3B) {
        throw new Error(
          `The 3B model requires the native engine, but it could not be loaded. ` +
          `Please ensure you have built the native components (npm run build:native).\n` +
          `Error: ${nativeErr}`
        );
      }

      // 2. FALLBACK TO WASM ENGINE (for 1.5B/0.5B models)
      console.warn(
        `[diffmind] Native engine unavailable, falling back to Wasm. ` +
        `If this is unexpected, rebuild with: npm run build:native\n` +
        `  Native error: ${nativeErr}`
      );
      try {
        const { ReviewAnalyzer: WasmAnalyzer } = require("@diffmind/core-wasm");
        let modelBytes: Buffer | null = fs.readFileSync(modelPath);
        const tokenizerBytes = fs.readFileSync(tokenizerPath);
        
        const wasmAnalyzer = new WasmAnalyzer(modelBytes, tokenizerBytes);
        
        // Manual GC trigger for Wasm heap management
        modelBytes = null;
        if (global.gc) global.gc();

        const analyzer: Analyzer = {
          analyze: async (diff, context, maxTokens) => {
            // Wasm version is synchronous, wrap in Promise for consistent interface
            return wasmAnalyzer.analyze_diff_chunked(diff, context, maxTokens);
          }
        };

        return { analyzer, engineType: "wasm" };
      } catch (wasmErr) {
        throw new Error(
          `Failed to load both Native and Wasm engines.\n` +
          `Native error: ${nativeErr}\n` +
          `Wasm error: ${wasmErr}`
        );
      }
    }
  }
}
