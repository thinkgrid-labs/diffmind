import { Indexer, type SymbolIndex } from "./indexer";

/**
 * Retrieves architectural context for a given git diff by mapping
 * symbols in the diff to their definitions in the local repository.
 */
export async function getRagContext(diff: string, manualIndex?: SymbolIndex | null): Promise<string | null> {
  const index = manualIndex || Indexer.load(process.cwd());
  if (!index) return null;

  const foundSymbols = new Set<string>();
  const lines = diff.split("\n");

  // Symbols to ignore to avoid noise in the retrieval loop
  const commonKeywords = new Set([
    "if", "else", "for", "while", "return", "const", "let", "var", 
    "function", "class", "interface", "type", "import", "export", 
    "from", "async", "await", "true", "false", "null", "undefined",
    "string", "number", "boolean", "any", "void", "Promise"
  ]);

  // Heuristic: find all words that match a known symbol name in added/modified lines
  for (const line of lines) {
    // Only look at added lines (+) but skip the file markers (+++)
    if (!line.startsWith("+") || line.startsWith("+++")) continue;

    const words = line.match(/[a-zA-Z0-9_$]+/g);
    if (!words) continue;

    for (const word of words) {
      if (index.symbols[word] && !commonKeywords.has(word)) {
        foundSymbols.add(word);
      }
    }
  }

  if (foundSymbols.size === 0) return null;

  let contexts = "";
  // Pick top 10 symbols to avoid prompt overflow
  const symbolsToInclude = Array.from(foundSymbols).slice(0, 10);
  
  for (const symName of symbolsToInclude) {
    const def = index.symbols[symName];
    contexts += `\n--- Arch Reference: ${symName} (from ${def.file}) ---\n${def.snippet}\n`;
  }

  return contexts;
}
