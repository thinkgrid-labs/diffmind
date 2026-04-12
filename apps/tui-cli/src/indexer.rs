use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use regex::Regex;
use chrono::Utc;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolDefinition {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub snippet: String,
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolIndex {
    pub version: String,
    pub project_root: String,
    pub updated_at: String,
    pub symbols: HashMap<String, SymbolDefinition>,
    pub file_mtimes: HashMap<String, f64>,
}

lazy_static::lazy_static! {
    pub static ref IGNORE_DIRS: HashSet<&'static str> = vec![
        "node_modules", ".git", "dist", "pkg", ".diffmind", "target"
    ].into_iter().collect();

    pub static ref EXTENSIONS: HashSet<&'static str> = vec![
        "ts", "tsx", "js", "jsx", "go", "py", "rs"
    ].into_iter().collect();

    pub static ref COMMON_KEYWORDS: HashSet<&'static str> = vec![
        "if", "else", "for", "while", "return", "const", "let", "var",
        "function", "class", "interface", "type", "import", "export",
        "from", "async", "await", "true", "false", "null", "undefined",
        "string", "number", "boolean", "any", "void", "Promise",
    ].into_iter().collect();
}

pub struct Indexer {
    project_root: PathBuf,
    symbols: HashMap<String, SymbolDefinition>,
}

impl Indexer {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            symbols: HashMap::new(),
        }
    }

    pub fn build_index(&mut self, existing: Option<SymbolIndex>) -> Result<SymbolIndex, anyhow::Error> {
        let mut file_mtimes = HashMap::new();
        
        if let Some(ref idx) = existing {
            self.symbols = idx.symbols.clone();
        }

        let old_mtimes = if let Some(ref idx) = existing {
            idx.file_mtimes.clone()
        } else {
            HashMap::new()
        };

        for entry in WalkDir::new(&self.project_root)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !IGNORE_DIRS.contains(name.as_ref())
            })
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let ext = entry.path().extension().and_then(|s| s.to_str()).unwrap_or("");
                if EXTENSIONS.contains(ext) {
                    let relative_path = entry.path().strip_prefix(&self.project_root)?.to_string_lossy().to_string();
                    let metadata = entry.metadata()?;
                    let mtime = metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs_f64();
                    
                    file_mtimes.insert(relative_path.clone(), mtime);

                    if Some(&mtime) != old_mtimes.get(&relative_path) {
                        self.parse_file(entry.path(), &relative_path)?;
                    }
                }
            }
        }

        // Clean up deleted files
        self.symbols.retain(|_, v| file_mtimes.contains_key(&v.file));

        Ok(SymbolIndex {
            version: "1.1.0".to_string(),
            project_root: self.project_root.to_string_lossy().to_string(),
            updated_at: Utc::now().to_rfc3339(),
            symbols: self.symbols.clone(),
            file_mtimes,
        })
    }

    fn parse_file(&mut self, absolute_path: &Path, relative_path: &str) -> Result<(), anyhow::Error> {
        let content = fs::read_to_string(absolute_path)?;
        let lines: Vec<&str> = content.lines().collect();

        let patterns = vec![
            ("function", Regex::new(r"export\s+(?:async\s+)?function\s+([a-zA-Z0-9_$]+)")?),
            ("class", Regex::new(r"export\s+class\s+([a-zA-Z0-9_$]+)")?),
            ("interface", Regex::new(r"export\s+interface\s+([a-zA-Z0-9_$]+)")?),
            ("type", Regex::new(r"export\s+type\s+([a-zA-Z0-9_$]+)")?),
            ("const", Regex::new(r"export\s+(?:const|let|var)\s+([a-zA-Z0-9_$]+)")?),
            // Go
            ("function", Regex::new(r"(?m)^func\s+([A-Z][a-zA-Z0-9_$]*)")?),
            ("interface", Regex::new(r"(?m)^type\s+([A-Z][a-zA-Z0-9_$]*)\s+interface")?),
            ("class", Regex::new(r"(?m)^type\s+([A-Z][a-zA-Z0-9_$]*)\s+struct")?),
            // Python
            ("function", Regex::new(r"(?m)^def\s+([a-zA-Z0-9_$]+)\(")?),
            ("class", Regex::new(r"(?m)^class\s+([a-zA-Z0-9_$]+)[(:]")?),
            // Rust
            ("function", Regex::new(r"pub\s+fn\s+([a-z0-9_]+)")?),
            ("class", Regex::new(r"pub\s+struct\s+([A-Z][a-zA-Z0-9]*)")?),
            ("interface", Regex::new(r"pub\s+trait\s+([A-Z][a-zA-Z0-9]*)")?),
        ];

        for (i, line) in lines.iter().enumerate() {
            // Optimization: skip lines that don't look like definitions
            if !line.contains("export") && !line.contains("pub ") && !line.starts_with("def ") && !line.starts_with("class ") && !line.starts_with("func ") && !line.starts_with("type ") {
                continue;
            }

            for (r#type, re) in &patterns {
                for cap in re.captures_iter(line) {
                    let name = &cap[1];
                    if self.symbols.contains_key(name) {
                        continue;
                    }

                    let snippet = self.extract_smart_snippet(&lines, i);
                    self.symbols.insert(name.to_string(), SymbolDefinition {
                        name: name.to_string(),
                        file: relative_path.to_string(),
                        line: i + 1,
                        r#type: r#type.to_string(),
                        snippet,
                    });
                }
            }
        }

        Ok(())
    }

    fn extract_smart_snippet(&self, lines: &[&str], start_line: usize) -> String {
        let mut brace_count = 0;
        let mut found_start_brace = false;
        let mut end_line = start_line;
        let max_lines = 40;

        for (i, line) in lines.iter().enumerate().skip(start_line).take(max_lines) {
            let (delta, has_open_brace) = count_braces_in_line(line);
            brace_count += delta;
            if has_open_brace { found_start_brace = true; }
            end_line = i;
            if found_start_brace && brace_count <= 0 { break; }
        }

        lines[start_line..=end_line].join("\n")
    }

    pub fn save(&self, index: &SymbolIndex) -> Result<(), anyhow::Error> {
        let dir = PathBuf::from(&index.project_root).join(".diffmind");
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let index_path = dir.join("symbols.json");
        fs::write(index_path, serde_json::to_string_pretty(index)?)?;
        Ok(())
    }

    pub fn load(project_root: &Path) -> Option<SymbolIndex> {
        let index_path = project_root.join(".diffmind").join("symbols.json");
        if !index_path.exists() {
            return None;
        }
        fs::read_to_string(index_path).ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }
}

fn count_braces_in_line(line: &str) -> (i32, bool) {
    let mut delta = 0;
    let mut has_open_brace = false;
    let mut in_string = false;
    let mut string_char = ' ';
    let mut escaped = false;

    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == string_char {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_string = true;
            string_char = ch;
            continue;
        }
        if ch == '{' {
            delta += 1;
            has_open_brace = true;
        } else if ch == '}' {
            delta -= 1;
        }
    }

    (delta, has_open_brace)
}
