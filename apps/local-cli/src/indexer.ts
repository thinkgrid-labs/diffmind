import * as fs from "fs";
import * as path from "path";

/**
 * Symbol definition found in the source code.
 */
export interface SymbolDefinition {
  name: string;
  file: string;
  line: number;
  snippet: string;
  type: "function" | "class" | "interface" | "type" | "const";
}

/**
 * Index of all symbols in the repository.
 */
export interface SymbolIndex {
  version: string;
  projectRoot: string;
  updatedAt: string;
  symbols: Record<string, SymbolDefinition>;
  fileMtimes: Record<string, number>;
}

const IGNORE_DIRS = new Set(["node_modules", ".git", "dist", "pkg", ".diffmind"]);
const EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx", ".go", ".py"]);

/**
 * Symbol Indexer
 * 
 * Crawls the project to find exported definitions. This forms the base
 * of the Local RAG system by providing architectural context to the AI.
 */
export class Indexer {
  private projectRoot: string;
  private symbols: Record<string, SymbolDefinition> = {};

  constructor(projectRoot: string) {
    this.projectRoot = projectRoot;
  }

  /**
   * Crawls the project and builds the symbol index.
   */
  public async buildIndex(existingIndex?: SymbolIndex | null): Promise<SymbolIndex> {
    const fileMtimes: Record<string, number> = {};
    
    // Copy existing symbols from files that haven't changed
    if (existingIndex) {
      this.symbols = { ...existingIndex.symbols };
    }

    await this.crawl(this.projectRoot, fileMtimes, existingIndex?.fileMtimes || {});
    
    // Clean up symbols for deleted files
    for (const symName in this.symbols) {
      if (!fileMtimes[this.symbols[symName].file]) {
        delete this.symbols[symName];
      }
    }

    return {
      version: "1.1.0",
      projectRoot: this.projectRoot,
      updatedAt: new Date().toISOString(),
      symbols: this.symbols,
      fileMtimes,
    };
  }

  private async crawl(
    dir: string, 
    newMtimes: Record<string, number>, 
    oldMtimes: Record<string, number>
  ): Promise<void> {
    const entries = fs.readdirSync(dir, { withFileTypes: true });

    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      const relativePath = path.relative(this.projectRoot, fullPath);

      if (entry.isDirectory()) {
        if (IGNORE_DIRS.has(entry.name)) continue;
        await this.crawl(fullPath, newMtimes, oldMtimes);
      } else if (entry.isFile()) {
        const ext = path.extname(entry.name);
        if (EXTENSIONS.has(ext)) {
          const stats = fs.statSync(fullPath);
          newMtimes[relativePath] = stats.mtimeMs;

          // Only parse if the file is new or modified
          if (stats.mtimeMs !== oldMtimes[relativePath]) {
            this.parseFile(fullPath, relativePath);
          }
        }
      }
    }
  }

  private parseFile(absolutePath: string, relativePath: string): void {
    const content = fs.readFileSync(absolutePath, "utf-8");
    const lines = content.split("\n");

    // Regex patterns for various export types
    // Note: We avoid complex parsing to keep it fast and zero-dependency.
    const patterns = [
      {
        type: "function" as const,
        regex: /export\s+(?:async\s+)?function\s+([a-zA-Z0-9_$]+)/g,
      },
      {
        type: "class" as const,
        regex: /export\s+class\s+([a-zA-Z0-9_$]+)/g,
      },
      {
        type: "interface" as const,
        regex: /export\s+interface\s+([a-zA-Z0-9_$]+)/g,
      },
      {
        type: "type" as const,
        regex: /export\s+type\s+([a-zA-Z0-9_$]+)/g,
      },
      {
        type: "const" as const,
        regex: /export\s+(?:const|let|var)\s+([a-zA-Z0-9_$]+)/g,
      },
      // Go patterns
      {
        type: "function" as const,
        regex: /^func\s+([A-Z][a-zA-Z0-9_$]*)/mg,
      },
      {
        type: "interface" as const,
        regex: /^type\s+([A-Z][a-zA-Z0-9_$]*)\s+interface/mg,
      },
      {
        type: "class" as const, // Treat structs as classes for AI review similarity
        regex: /^type\s+([A-Z][a-zA-Z0-9_$]*)\s+struct/mg,
      },
      // Python patterns (top-level only)
      {
        type: "function" as const,
        regex: /^def\s+([a-zA-Z0-9_$]+)\(/mg,
      },
      {
        type: "class" as const,
        regex: /^class\s+([a-zA-Z0-9_$]+)(?:\(|\:)/mg,
      },
    ];

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      // Quick optimization: only scan lines that look like definitions
      if (!line.includes("export") && !line.startsWith("def ") && !line.startsWith("class ") && !line.startsWith("func ") && !line.startsWith("type ")) {
        continue;
      }

      for (const pattern of patterns) {
        let match;
        pattern.regex.lastIndex = 0;
        
        while ((match = pattern.regex.exec(line)) !== null) {
          const name = match[1];
          const snippet = this.extractSmartSnippet(lines, i);

          this.symbols[name] = {
            name,
            file: relativePath,
            line: i + 1,
            type: pattern.type,
            snippet,
          };
        }
      }
    }
  }

  private extractSmartSnippet(lines: string[], startLine: number): string {
    let braceCount = 0;
    let foundStartBrace = false;
    let endLine = startLine;
    const maxLines = 40;

    for (let i = startLine; i < Math.min(startLine + maxLines, lines.length); i++) {
      const line = lines[i];
      
      // Count braces
      for (const char of line) {
        if (char === "{") {
          braceCount++;
          foundStartBrace = true;
        } else if (char === "}") {
          braceCount--;
        }
      }

      endLine = i;

      // If we found braces and now they are balanced, we found the end
      if (foundStartBrace && braceCount <= 0) {
        break;
      }
    }

    return lines.slice(startLine, endLine + 1).join("\n");
  }

  /**
   * Persists the index to the .diffmind directory.
   */
  public save(index: SymbolIndex): void {
    const dir = path.join(this.projectRoot, ".diffmind");
    if (!fs.existsSync(dir)) {
      fs.mkdirSync(dir, { recursive: true });
    }

    const indexPath = path.join(dir, "symbols.json");
    fs.writeFileSync(indexPath, JSON.stringify(index, null, 2), "utf-8");
  }

  /**
   * Loads the index from disk if it exists.
   */
  public static load(projectRoot: string): SymbolIndex | null {
    const indexPath = path.join(projectRoot, ".diffmind", "symbols.json");
    if (!fs.existsSync(indexPath)) return null;

    try {
      return JSON.parse(fs.readFileSync(indexPath, "utf-8"));
    } catch {
      return null;
    }
  }
}
