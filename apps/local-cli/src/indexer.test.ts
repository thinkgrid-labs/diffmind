import { Indexer } from "./indexer";
import * as fs from "fs";
import * as path from "path";

describe("Indexer", () => {
  const testRoot = path.join(__dirname, "test-repo");

  beforeAll(() => {
    if (!fs.existsSync(testRoot)) {
      fs.mkdirSync(testRoot, { recursive: true });
    }
    
    // Create a mock TypeScript file with various exports
    const content = `
export function add(a: number, b: number) { return a + b; }
export async function fetchUser(id: string) { return { id }; }
export class ReviewAnalyzer { analyze() {} }
export interface User { id: string; }
export type Severity = "high" | "low";
export const VERSION = "1.0.0";
`;
    fs.writeFileSync(path.join(testRoot, "index.ts"), content);
  });

  afterAll(() => {
    fs.rmSync(testRoot, { recursive: true, force: true });
  });

  it("should match all supported export types", async () => {
    const indexer = new Indexer(testRoot);
    const index = await indexer.buildIndex();

    expect(index.symbols["add"]).toBeDefined();
    expect(index.symbols["add"].type).toBe("function");
    
    expect(index.symbols["fetchUser"]).toBeDefined();
    expect(index.symbols["fetchUser"].type).toBe("function");

    expect(index.symbols["ReviewAnalyzer"]).toBeDefined();
    expect(index.symbols["ReviewAnalyzer"].type).toBe("class");

    expect(index.symbols["User"]).toBeDefined();
    expect(index.symbols["User"].type).toBe("interface");

    expect(index.symbols["Severity"]).toBeDefined();
    expect(index.symbols["Severity"].type).toBe("type");

    expect(index.symbols["VERSION"]).toBeDefined();
    expect(index.symbols["VERSION"].type).toBe("const");
  });

  it("should include code snippets in the index", async () => {
    const indexer = new Indexer(testRoot);
    const index = await indexer.buildIndex();

    expect(index.symbols["add"].snippet).toContain("return a + b;");
    expect(index.symbols["add"].snippet).toContain("}");
  });

  it("should support Go and Python symbols", async () => {
    const goPath = path.join(testRoot, "main.go");
    const pyPath = path.join(testRoot, "lib.py");

    fs.writeFileSync(goPath, "func Calculator(a int) {\n  return a\n}");
    fs.writeFileSync(pyPath, "def run_analysis(data):\n    print(data)\n    return True");

    const indexer = new Indexer(testRoot);
    const index = await indexer.buildIndex();

    expect(index.symbols["Calculator"]).toBeDefined();
    expect(index.symbols["Calculator"].file).toBe("main.go");
    expect(index.symbols["run_analysis"]).toBeDefined();
    expect(index.symbols["run_analysis"].file).toBe("lib.py");
  });

  it("should skip files with unchanged mtimes (incremental)", async () => {
    const indexer = new Indexer(testRoot);
    const initialIndex = await indexer.buildIndex();
    
    // Spy on parseFile to count calls
    const parseSpy = jest.spyOn(indexer as any, "parseFile");
    
    // Second run with existing index
    await indexer.buildIndex(initialIndex);
    
    // Should NOT have called parseFile because mtimes are the same
    expect(parseSpy).not.toHaveBeenCalled();
    parseSpy.mockRestore();
  });

  it("should ignore braces inside strings during extraction", async () => {
    const content = `
export function complex() {
  const str = "{ fake brace }";
  return { real: true };
}
`;
    fs.writeFileSync(path.join(testRoot, "complex.ts"), content);
    const indexer = new Indexer(testRoot);
    const index = await indexer.buildIndex();
    
    const snippet = index.symbols["complex"].snippet;
    expect(snippet).toContain("return { real: true };");
    expect(snippet).toContain("}");
    // Verify it didn't stop early at the string's brace
    expect(snippet.split("\n").length).toBeGreaterThan(3);
  });
});
