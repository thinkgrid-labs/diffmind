import { Indexer, type SymbolIndex } from "./indexer";

const COMMON_KEYWORDS = new Set([
  "if", "else", "for", "while", "return", "const", "let", "var",
  "function", "class", "interface", "type", "import", "export",
  "from", "async", "await", "true", "false", "null", "undefined",
  "string", "number", "boolean", "any", "void", "Promise",
]);

// Cap total byte size so the assembled context never overflows the model's
// token budget when injected into the prompt. 3 000 bytes ≈ 750 tokens.
const MAX_CONTEXT_BYTES = 3000;

/** Scans added diff lines and returns symbol names that exist in the index. */
function extractSymbolsFromDiff(diff: string, index: SymbolIndex): Set<string> {
  const found = new Set<string>();
  for (const line of diff.split("\n")) {
    if (!line.startsWith("+") || line.startsWith("+++")) continue;
    const words = line.match(/[a-zA-Z0-9_$]+/g);
    if (!words) continue;
    for (const word of words) {
      if (index.symbols[word] && !COMMON_KEYWORDS.has(word)) {
        found.add(word);
      }
    }
  }
  return found;
}

/** Assembles snippet entries up to MAX_CONTEXT_BYTES, returning complete entries only. */
function buildContextString(symbols: string[], index: SymbolIndex): string {
  let result = "";
  for (const name of symbols) {
    const def = index.symbols[name];
    const entry = `\n--- Arch Reference: ${name} (from ${def.file}) ---\n${def.snippet}\n`;
    if (Buffer.byteLength(result + entry, "utf-8") > MAX_CONTEXT_BYTES) break;
    result += entry;
  }
  return result;
}

/**
 * Retrieves architectural context for a given git diff by mapping
 * symbols in the diff to their definitions in the local repository.
 */
export async function getRagContext(diff: string, manualIndex?: SymbolIndex | null): Promise<string | null> {
  const index = manualIndex || Indexer.load(process.cwd());
  if (!index) return null;

  const foundSymbols = extractSymbolsFromDiff(diff, index);
  if (foundSymbols.size === 0) return null;

  const symbolsToInclude = Array.from(foundSymbols).slice(0, 10);
  const context = buildContextString(symbolsToInclude, index);
  return context || null;
}
