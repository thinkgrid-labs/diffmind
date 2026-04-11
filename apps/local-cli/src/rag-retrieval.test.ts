import { getRagContext } from "./rag";
import { type SymbolIndex } from "./indexer";

// Mock chalk for tests
jest.mock("chalk", () => ({
  red: { bold: (s: string) => s },
  yellow: { bold: (s: string) => s },
  blue: { bold: (s: string) => s },
  cyan: (s: string) => s,
  gray: (s: string) => s,
  bold: (s: string) => s,
  dim: (s: string) => s,
}));

describe("RAG Retrieval", () => {
  const mockIndex: SymbolIndex = {
    version: "1.1.0",
    projectRoot: "/test",
    updatedAt: new Date().toISOString(),
    fileMtimes: {},
    symbols: {
      "AuthService": {
        name: "AuthService",
        file: "src/auth.ts",
        line: 1,
        type: "class",
        snippet: "export class AuthService {}"
      },
      "validateToken": {
        name: "validateToken",
        file: "src/utils.ts",
        line: 10,
        type: "function",
        snippet: "export function validateToken() {}"
      }
    }
  };

  it("should extract symbols from a git diff", async () => {
    const diff = `
+ import { AuthService } from "./auth";
+ const service = new AuthService();
+ validateToken();
`;
    const context = await getRagContext(diff, mockIndex);
    
    expect(context).toContain("Arch Reference: AuthService");
    expect(context).toContain("Arch Reference: validateToken");
    expect(context).toContain("export class AuthService {}");
  });

  it("should ignore common language keywords", async () => {
    // If we happen to have a symbol named 'return' (unlikely but possible in some projects)
    // our filter should still ignore it if it's in the commonKeywords list.
    const diff = `
+ return true;
`;
    const context = await getRagContext(diff, mockIndex);
    expect(context).toBeNull();
  });

  // it("should ignore removed lines", async () => {
  //   const diff = `
  // - AuthService.login();
  // + console.log("removed");
  // `;
  //   const context = await getRagContext(diff, mockIndex);
  //   expect(context).toBeNull();
  // });

  it("should limit the number of retrieved symbols to 10", async () => {
    const multiSymbolIndex: SymbolIndex = { ...mockIndex, symbols: {} };
    for (let i = 0; i < 20; i++) {
       multiSymbolIndex.symbols[`Sym${i}`] = {
         name: `Sym${i}`, file: "f.ts", line: 1, type: "const", snippet: "s"
       };
    }

    const diff = Array.from({length: 20}, (_, i) => `+ Sym${i}`).join("\n");
    const context = await getRagContext(diff, multiSymbolIndex);
    
    // Count occurrences of "Arch Reference"
    const matches = (context?.match(/Arch Reference/g) || []).length;
    expect(matches).toBe(10);
  });
});
