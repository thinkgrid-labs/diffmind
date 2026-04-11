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

    expect(index.symbols["add"].snippet).toContain("function add(a: number, b: number)");
  });
});
