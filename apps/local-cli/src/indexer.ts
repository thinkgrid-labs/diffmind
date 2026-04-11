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
}

const IGNORE_DIRS = new Set(["node_modules", ".git", "dist", "pkg", ".diffmind"]);
const EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx"]);

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
  public async buildIndex(): Promise<SymbolIndex> {
    await this.crawl(this.projectRoot);
    
    return {
      version: "1.0.0",
      projectRoot: this.projectRoot,
      updatedAt: new Error().stack?.includes("sync") ? new Date().toISOString() : new Date().toISOString(),
      symbols: this.symbols,
    };
  }

  private async crawl(dir: string): Promise<void> {
    const entries = fs.readdirSync(dir, { withFileTypes: true });

    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      const relativePath = path.relative(this.projectRoot, fullPath);

      if (entry.isDirectory()) {
        if (IGNORE_DIRS.has(entry.name)) continue;
        await this.crawl(fullPath);
      } else if (entry.isFile()) {
        if (EXTENSIONS.has(path.extname(entry.name))) {
          this.parseFile(fullPath, relativePath);
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
    ];

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      if (!line.includes("export")) continue;

      for (const pattern of patterns) {
        let match;
        // Important: Reset lastIndex for global regex
        pattern.regex.lastIndex = 0;
        
        while ((match = pattern.regex.exec(line)) !== null) {
          const name = match[1];
          // Take 15 lines of context for the snippet
          const snippet = lines.slice(i, i + 15).join("\n");

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
