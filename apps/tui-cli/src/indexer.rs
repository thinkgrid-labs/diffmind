use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Bumped when the on-disk shape changes so a stale index is rebuilt rather
/// than half-deserialized.
const INDEX_VERSION: &str = "2.0.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolDefinition {
    pub name: String,
    pub file: String,
    pub line: usize,
    /// Last line of the definition, used to find the symbol enclosing a hunk.
    #[serde(default)]
    pub end_line: usize,
    pub snippet: String,
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolIndex {
    pub version: String,
    pub project_root: String,
    pub updated_at: String,
    /// Symbol name → every definition with that name.
    ///
    /// This was a flat `HashMap<String, SymbolDefinition>` where the first
    /// definition encountered won, so a common name like `Config` or `handler`
    /// silently shadowed every other one in the repo and the model was handed
    /// the wrong body.
    pub symbols: HashMap<String, Vec<SymbolDefinition>>,
    pub file_mtimes: HashMap<String, f64>,
}

impl SymbolIndex {
    pub fn symbol_count(&self) -> usize {
        self.symbols.values().map(Vec::len).sum()
    }

    /// Definitions of `name`, preferring one in `near_file` when the name is
    /// ambiguous — a local definition is far likelier to be the referent.
    pub fn lookup(&self, name: &str, near_file: Option<&str>) -> Option<&SymbolDefinition> {
        let defs = self.symbols.get(name)?;
        if let Some(file) = near_file
            && let Some(local) = defs.iter().find(|d| d.file == file)
        {
            return Some(local);
        }
        defs.first()
    }

    /// The definition whose body contains `line` in `file` — the function a
    /// diff hunk landed inside.
    pub fn enclosing(&self, file: &str, line: usize) -> Option<&SymbolDefinition> {
        self.symbols
            .values()
            .flatten()
            .filter(|d| d.file == file && d.line <= line && line <= d.end_line.max(d.line))
            // Innermost wins when definitions nest.
            .min_by_key(|d| d.end_line.saturating_sub(d.line))
    }
}

lazy_static::lazy_static! {
    pub static ref IGNORE_DIRS: HashSet<&'static str> = vec![
        "node_modules", ".git", "dist", "pkg", ".diffmind", "target",
        "build", ".next", ".cache", "vendor", "__pycache__", ".venv", "venv",
    ].into_iter().collect();

    pub static ref EXTENSIONS: HashSet<&'static str> = vec![
        "ts", "tsx", "js", "jsx", "go", "py", "rs"
    ].into_iter().collect();

    pub static ref COMMON_KEYWORDS: HashSet<&'static str> = vec![
        "if", "else", "for", "while", "return", "const", "let", "var",
        "function", "class", "interface", "type", "import", "export",
        "from", "async", "await", "true", "false", "null", "undefined",
        "string", "number", "boolean", "any", "void", "Promise",
        "self", "this", "new", "match", "impl", "fn", "pub", "mut", "struct",
        "trait", "enum", "use", "mod", "def", "pass", "None", "True", "False",
    ].into_iter().collect();
}

/// Definition patterns, compiled once rather than on every file.
struct Patterns {
    entries: Vec<(&'static str, Regex)>,
}

impl Patterns {
    fn new() -> Result<Self> {
        let entries = vec![
            // TypeScript / JavaScript
            (
                "function",
                Regex::new(r"export\s+(?:async\s+)?function\s+([a-zA-Z0-9_$]+)")?,
            ),
            (
                "class",
                Regex::new(r"export\s+(?:abstract\s+)?class\s+([a-zA-Z0-9_$]+)")?,
            ),
            (
                "interface",
                Regex::new(r"export\s+interface\s+([a-zA-Z0-9_$]+)")?,
            ),
            ("type", Regex::new(r"export\s+type\s+([a-zA-Z0-9_$]+)")?),
            (
                "const",
                Regex::new(r"export\s+(?:const|let|var)\s+([a-zA-Z0-9_$]+)")?,
            ),
            // Go
            (
                "function",
                Regex::new(r"(?m)^func\s+(?:\([^)]*\)\s*)?([A-Z][a-zA-Z0-9_$]*)")?,
            ),
            (
                "interface",
                Regex::new(r"(?m)^type\s+([A-Z][a-zA-Z0-9_$]*)\s+interface")?,
            ),
            (
                "class",
                Regex::new(r"(?m)^type\s+([A-Z][a-zA-Z0-9_$]*)\s+struct")?,
            ),
            // Python
            ("function", Regex::new(r"(?m)^\s*def\s+([a-zA-Z0-9_]+)\(")?),
            ("class", Regex::new(r"(?m)^\s*class\s+([a-zA-Z0-9_]+)[(:]")?),
            // Rust
            (
                "function",
                Regex::new(r"pub\s+(?:async\s+)?fn\s+([a-z0-9_]+)")?,
            ),
            ("class", Regex::new(r"pub\s+struct\s+([A-Z][a-zA-Z0-9]*)")?),
            ("enum", Regex::new(r"pub\s+enum\s+([A-Z][a-zA-Z0-9]*)")?),
            (
                "interface",
                Regex::new(r"pub\s+trait\s+([A-Z][a-zA-Z0-9]*)")?,
            ),
        ];
        Ok(Patterns { entries })
    }
}

pub struct Indexer {
    project_root: PathBuf,
    symbols: HashMap<String, Vec<SymbolDefinition>>,
    patterns: Patterns,
}

impl Indexer {
    pub fn new(project_root: PathBuf) -> Result<Self> {
        Ok(Self {
            project_root,
            symbols: HashMap::new(),
            patterns: Patterns::new()?,
        })
    }

    pub fn build_index(&mut self, existing: Option<SymbolIndex>) -> Result<SymbolIndex> {
        let mut file_mtimes = HashMap::new();

        // Only reuse a prior index if it was written by this format version.
        let existing = existing.filter(|i| i.version == INDEX_VERSION);

        if let Some(ref idx) = existing {
            self.symbols = idx.symbols.clone();
        }
        let old_mtimes = existing.map(|i| i.file_mtimes).unwrap_or_default();

        for entry in WalkDir::new(&self.project_root)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !IGNORE_DIRS.contains(name.as_ref()) && !name.starts_with('.') || e.depth() == 0
            })
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry
                .path()
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !EXTENSIONS.contains(ext) {
                continue;
            }

            let Ok(relative) = entry.path().strip_prefix(&self.project_root) else {
                continue;
            };
            let relative_path = relative.to_string_lossy().replace('\\', "/");

            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
                continue;
            };
            let mtime = since_epoch.as_secs_f64();

            file_mtimes.insert(relative_path.clone(), mtime);

            if Some(&mtime) != old_mtimes.get(&relative_path) {
                // Drop this file's previous symbols before reparsing.
                //
                // Without this, `parse_file` skipped any name already in the
                // map — which after loading the existing index meant *every*
                // name — so a changed definition was never refreshed and the
                // only fix was deleting symbols.json by hand.
                self.forget_file(&relative_path);
                if let Err(e) = self.parse_file(entry.path(), &relative_path) {
                    eprintln!("  !  could not index {relative_path}: {e}");
                }
            }
        }

        // Clean up deleted files.
        for defs in self.symbols.values_mut() {
            defs.retain(|d| file_mtimes.contains_key(&d.file));
        }
        self.symbols.retain(|_, defs| !defs.is_empty());

        Ok(SymbolIndex {
            version: INDEX_VERSION.to_string(),
            project_root: self.project_root.to_string_lossy().to_string(),
            updated_at: Utc::now().to_rfc3339(),
            symbols: self.symbols.clone(),
            file_mtimes,
        })
    }

    fn forget_file(&mut self, relative_path: &str) {
        for defs in self.symbols.values_mut() {
            defs.retain(|d| d.file != relative_path);
        }
        self.symbols.retain(|_, defs| !defs.is_empty());
    }

    fn parse_file(&mut self, absolute_path: &Path, relative_path: &str) -> Result<()> {
        let content = fs::read_to_string(absolute_path)?;
        let lines: Vec<&str> = content.lines().collect();

        for (i, line) in lines.iter().enumerate() {
            // Cheap pre-filter: skip lines that cannot be a definition.
            if !line.contains("export")
                && !line.contains("pub ")
                && !line.trim_start().starts_with("def ")
                && !line.trim_start().starts_with("class ")
                && !line.starts_with("func ")
                && !line.starts_with("type ")
            {
                continue;
            }

            for (kind, re) in &self.patterns.entries {
                for cap in re.captures_iter(line) {
                    let name = &cap[1];
                    let (snippet, end_line) = extract_snippet(&lines, i);

                    let defs = self.symbols.entry(name.to_string()).or_default();
                    // Same name, same file, same line = the same symbol matched
                    // by two overlapping patterns.
                    if defs
                        .iter()
                        .any(|d| d.file == relative_path && d.line == i + 1)
                    {
                        continue;
                    }
                    defs.push(SymbolDefinition {
                        name: name.to_string(),
                        file: relative_path.to_string(),
                        line: i + 1,
                        end_line: end_line + 1,
                        r#type: kind.to_string(),
                        snippet,
                    });
                }
            }
        }

        Ok(())
    }

    pub fn save(&self, index: &SymbolIndex) -> Result<()> {
        let dir = PathBuf::from(&index.project_root).join(".diffmind");
        fs::create_dir_all(&dir)?;
        let path = dir.join("symbols.json");
        let tmp = dir.join("symbols.json.tmp");
        fs::write(&tmp, serde_json::to_string(index)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load(project_root: &Path) -> Option<SymbolIndex> {
        let path = project_root.join(".diffmind").join("symbols.json");
        let raw = fs::read_to_string(path).ok()?;
        let index: SymbolIndex = serde_json::from_str(&raw).ok()?;
        // An index from an older layout is worse than none: it would feed the
        // model definitions in a shape this build no longer understands.
        (index.version == INDEX_VERSION).then_some(index)
    }
}

/// Body of the definition starting at `start_line`, plus the line it ends on.
///
/// Brace-counting handles the C family and Go; indentation handles Python,
/// which has no braces to count and previously fell through to a 40-line
/// window regardless of the function's real size.
fn extract_snippet(lines: &[&str], start_line: usize) -> (String, usize) {
    const MAX_LINES: usize = 60;

    let is_python_style = {
        let t = lines[start_line].trim_start();
        (t.starts_with("def ") || t.starts_with("class "))
            && lines[start_line].trim_end().ends_with(':')
    };

    let end_line = if is_python_style {
        python_block_end(lines, start_line, MAX_LINES)
    } else {
        brace_block_end(lines, start_line, MAX_LINES)
    };

    (lines[start_line..=end_line].join("\n"), end_line)
}

fn brace_block_end(lines: &[&str], start_line: usize, max_lines: usize) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    let mut end = start_line;

    for (i, line) in lines.iter().enumerate().skip(start_line).take(max_lines) {
        let (delta, has_open) = count_braces_in_line(line);
        depth += delta;
        if has_open {
            opened = true;
        }
        end = i;
        if opened && depth <= 0 {
            break;
        }
        // A single-line declaration with no braces at all (a type alias, a
        // const) ends where it starts.
        if !opened && i > start_line && line.trim().is_empty() {
            end = i - 1;
            break;
        }
    }
    end
}

fn python_block_end(lines: &[&str], start_line: usize, max_lines: usize) -> usize {
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    let base = indent_of(lines[start_line]);
    let mut end = start_line;

    for (i, line) in lines
        .iter()
        .enumerate()
        .skip(start_line + 1)
        .take(max_lines)
    {
        if line.trim().is_empty() {
            continue;
        }
        if indent_of(line) <= base {
            break;
        }
        end = i;
    }
    end
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-idx-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_changed_definition_is_refreshed_on_reindex() {
        let dir = tmpdir("refresh");
        let file = dir.join("a.ts");
        fs::write(&file, "export function greet() {\n  return 'v1';\n}\n").unwrap();

        let mut indexer = Indexer::new(dir.clone()).unwrap();
        let first = indexer.build_index(None).unwrap();
        assert!(first.lookup("greet", None).unwrap().snippet.contains("v1"));

        // Rewrite with a distinct mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&file, "export function greet() {\n  return 'v2';\n}\n").unwrap();

        let mut indexer = Indexer::new(dir.clone()).unwrap();
        let second = indexer.build_index(Some(first)).unwrap();
        assert!(
            second.lookup("greet", None).unwrap().snippet.contains("v2"),
            "an incremental reindex must pick up the new body"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_name_in_two_files_keeps_both_definitions() {
        let dir = tmpdir("collide");
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("a/x.ts"), "export type Config = { a: string };\n").unwrap();
        fs::write(dir.join("b/y.ts"), "export type Config = { b: number };\n").unwrap();

        let mut indexer = Indexer::new(dir.clone()).unwrap();
        let index = indexer.build_index(None).unwrap();

        let defs = index
            .symbols
            .get("Config")
            .expect("Config should be indexed");
        assert_eq!(defs.len(), 2, "one definition must not shadow the other");

        // Lookup prefers a definition in the file we are reviewing.
        let near = index.lookup("Config", Some("b/y.ts")).unwrap();
        assert_eq!(near.file, "b/y.ts");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_files_drop_out_of_the_index() {
        let dir = tmpdir("delete");
        fs::write(dir.join("gone.ts"), "export const willVanish = 1;\n").unwrap();

        let mut indexer = Indexer::new(dir.clone()).unwrap();
        let first = indexer.build_index(None).unwrap();
        assert!(first.lookup("willVanish", None).is_some());

        fs::remove_file(dir.join("gone.ts")).unwrap();
        let mut indexer = Indexer::new(dir.clone()).unwrap();
        let second = indexer.build_index(Some(first)).unwrap();
        assert!(second.lookup("willVanish", None).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enclosing_finds_the_function_a_line_sits_in() {
        let dir = tmpdir("enclosing");
        fs::write(
            dir.join("a.rs"),
            "pub fn outer() {\n    let x = 1;\n    let y = 2;\n}\n\npub fn other() {\n    ok();\n}\n",
        )
        .unwrap();

        let mut indexer = Indexer::new(dir.clone()).unwrap();
        let index = indexer.build_index(None).unwrap();

        assert_eq!(
            index.enclosing("a.rs", 2).map(|d| d.name.as_str()),
            Some("outer")
        );
        assert_eq!(
            index.enclosing("a.rs", 7).map(|d| d.name.as_str()),
            Some("other")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn python_snippets_end_at_the_dedent() {
        let src = vec![
            "def handler(req):",
            "    a = 1",
            "    return a",
            "",
            "def other():",
            "    pass",
        ];
        let (snippet, end) = extract_snippet(&src, 0);
        assert_eq!(end, 2, "should stop before the blank line and next def");
        assert!(snippet.contains("return a"));
        assert!(!snippet.contains("def other"));
    }

    #[test]
    fn an_index_from_an_older_version_is_rejected() {
        let dir = tmpdir("version");
        fs::create_dir_all(dir.join(".diffmind")).unwrap();
        fs::write(
            dir.join(".diffmind/symbols.json"),
            r#"{"version":"1.1.0","project_root":"/x","updated_at":"","symbols":{},"file_mtimes":{}}"#,
        )
        .unwrap();
        assert!(
            Indexer::load(&dir).is_none(),
            "a stale layout must be rebuilt, not half-read"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
