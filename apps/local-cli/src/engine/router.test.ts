import { EngineRouter } from "./router";
import * as fs from "fs";

// Mocking dependencies
jest.mock("fs");
const mockedFs = fs as jest.Mocked<typeof fs>;

// Mocking the engines which might not be built during tests
jest.mock("@diffmind/core-native", () => ({
  ReviewAnalyzer: jest.fn().mockImplementation(() => ({
    analyzeDiffChunked: jest.fn().mockResolvedValue("[native result]")
  }))
}), { virtual: true });

jest.mock("@diffmind/core-wasm", () => ({
  ReviewAnalyzer: jest.fn().mockImplementation(() => ({
    analyze_diff_chunked: jest.fn().mockReturnValue("[wasm result]")
  }))
}), { virtual: true });

describe("EngineRouter", () => {
  beforeEach(() => {
    jest.clearAllMocks();
    mockedFs.readFileSync.mockReturnValue(Buffer.from("dummy data"));
  });

  it("should select the native engine when modelId is '3b'", async () => {
    const { analyzer, engineType } = await EngineRouter.getAnalyzer(
      "3b",
      "model.gguf",
      "tokenizer.json"
    );

    expect(engineType).toBe("native");
    const result = await analyzer.analyze("diff", "context", 100);
    expect(result).toBe("[native result]");
  });

  it("should fallback to wasm for 1.5b if native is missing", async () => {
    // Force native to fail loading
    const coreNative = require("@diffmind/core-native");
    (coreNative.ReviewAnalyzer as jest.Mock).mockImplementationOnce(() => {
      throw new Error("Native load failed");
    });

    const { analyzer, engineType } = await EngineRouter.getAnalyzer(
      "1.5b",
      "model.gguf",
      "tokenizer.json"
    );

    expect(engineType).toBe("wasm");
    const result = await analyzer.analyze("diff", "context", 100);
    expect(result).toBe("[wasm result]");
  });

  it("should throw error for 3b if native engine is missing", async () => {
    const coreNative = require("@diffmind/core-native");
    (coreNative.ReviewAnalyzer as jest.Mock).mockImplementationOnce(() => {
      throw new Error("Native load failed");
    });

    await expect(
      EngineRouter.getAnalyzer("3b", "model.gguf", "tokenizer.json")
    ).rejects.toThrow(/requires the native engine/);
  });
});
