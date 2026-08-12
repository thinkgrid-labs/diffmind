//! The graph on disk — `.diffmind/graph.db`.
//!
//! SQLite rather than the JSON file it replaces, for one reason: references.
//! Definitions are small enough to hold in memory, but a repository of any size
//! has an order of magnitude more *mentions* of symbols than declarations of
//! them, and the whole point of the graph is the reverse lookup — "what calls
//! this?" — which needs an index, not a linear scan of a map loaded wholesale
//! on every review.
//!
//! Bodies are **not** stored. A definition records its line range and the source
//! is read from the working tree when needed, so the database stays small and
//! can never serve a snippet that no longer matches the file.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::extract::{self, Lang};

/// Bumped when the schema changes. A mismatch rebuilds from scratch rather than
/// querying a shape this build does not understand.
const SCHEMA_VERSION: i64 = 1;

/// Directories never worth walking.
const IGNORE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    "pkg",
];

/// A symbol, located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl Def {
    /// The declaration's source, read from the working tree and capped.
    ///
    /// Read rather than stored: a snippet in the database is a copy that goes
    /// stale, and staleness here means showing the model code that is not what
    /// it is reviewing.
    pub fn source(&self, project_root: &Path, max_lines: usize) -> Option<String> {
        let text = std::fs::read_to_string(project_root.join(&self.path)).ok()?;
        let lines: Vec<&str> = text.lines().collect();
        let start = (self.start_line as usize).saturating_sub(1);
        if start >= lines.len() {
            return None;
        }
        let end = (self.end_line as usize).min(lines.len());
        let end = end.min(start + max_lines);
        Some(lines[start..end].join("\n"))
    }

    fn span(&self) -> u32 {
        self.end_line.saturating_sub(self.start_line)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_unchanged: usize,
    pub files_removed: usize,
    pub definitions: usize,
    pub references: usize,
}

pub struct Graph {
    conn: Connection,
}

impl Graph {
    pub fn path(project_root: &Path) -> PathBuf {
        project_root.join(".diffmind").join("graph.db")
    }

    /// Open (creating if needed). A schema from another version is discarded —
    /// rebuilding costs seconds; querying a shape we misunderstand costs trust.
    pub fn open(project_root: &Path) -> Result<Graph> {
        let path = Self::path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS refs;
                 DROP TABLE IF EXISTS defs;
                 DROP TABLE IF EXISTS files;",
            )?;
        }

        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS files (
                 path   TEXT PRIMARY KEY,
                 mtime  REAL NOT NULL
             );
             CREATE TABLE IF NOT EXISTS defs (
                 path       TEXT NOT NULL,
                 name       TEXT NOT NULL,
                 kind       TEXT NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS refs (
                 path TEXT NOT NULL,
                 name TEXT NOT NULL,
                 line INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS defs_name ON defs(name);
             CREATE INDEX IF NOT EXISTS defs_path ON defs(path);
             CREATE INDEX IF NOT EXISTS refs_name ON refs(name);
             CREATE INDEX IF NOT EXISTS refs_path ON refs(path);
             PRAGMA user_version = {SCHEMA_VERSION};"
        ))?;

        Ok(Graph { conn })
    }

    /// Walk the project and bring the graph up to date, reparsing only files
    /// whose mtime moved.
    pub fn index(&mut self, project_root: &Path, on_file: &dyn Fn(usize)) -> Result<IndexStats> {
        let mut stats = IndexStats::default();
        let mut seen: HashSet<String> = HashSet::new();
        let tx = self.conn.transaction()?;

        for entry in walkdir::WalkDir::new(project_root)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !IGNORE_DIRS.contains(&name.as_ref()) && !name.starts_with('.')
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let Some(lang) = Lang::for_path(entry.path()) else {
                continue;
            };
            let Ok(relative) = entry.path().strip_prefix(project_root) else {
                continue;
            };
            let path = relative.to_string_lossy().replace('\\', "/");

            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);

            seen.insert(path.clone());

            let known: Option<f64> = tx
                .query_row("SELECT mtime FROM files WHERE path = ?1", [&path], |r| {
                    r.get(0)
                })
                .ok();
            if known == Some(mtime) {
                stats.files_unchanged += 1;
                continue;
            }

            let Ok(source) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Some(extracted) = extract::extract(lang, &source) else {
                continue;
            };

            // Replace this file's rows wholesale. Anything subtler would have
            // to diff two symbol sets, and a stale definition is worse than a
            // re-inserted one.
            tx.execute("DELETE FROM defs WHERE path = ?1", [&path])?;
            tx.execute("DELETE FROM refs WHERE path = ?1", [&path])?;

            {
                let mut insert_def = tx.prepare(
                    "INSERT INTO defs (path, name, kind, start_line, end_line) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for d in &extracted.definitions {
                    insert_def.execute(params![path, d.name, d.kind, d.start_line, d.end_line])?;
                }
                let mut insert_ref =
                    tx.prepare("INSERT INTO refs (path, name, line) VALUES (?1, ?2, ?3)")?;
                for r in &extracted.references {
                    insert_ref.execute(params![path, r.name, r.line])?;
                }
            }

            tx.execute(
                "INSERT INTO files (path, mtime) VALUES (?1, ?2) \
                 ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime",
                params![path, mtime],
            )?;

            stats.files_indexed += 1;
            stats.definitions += extracted.definitions.len();
            stats.references += extracted.references.len();
            on_file(stats.files_indexed);
        }

        // Files that have gone. Left behind, they would keep answering queries
        // about code that no longer exists.
        let stale: Vec<String> = {
            let mut q = tx.prepare("SELECT path FROM files")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok())
                .filter(|p| !seen.contains(p))
                .collect()
        };
        for path in &stale {
            tx.execute("DELETE FROM defs WHERE path = ?1", [path])?;
            tx.execute("DELETE FROM refs WHERE path = ?1", [path])?;
            tx.execute("DELETE FROM files WHERE path = ?1", [path])?;
        }
        stats.files_removed = stale.len();

        tx.commit()?;
        Ok(stats)
    }

    /// Definitions of `name`, preferring one in `near` when the name is
    /// ambiguous — a local declaration is far likelier to be the referent.
    pub fn definitions_of(&self, name: &str, near: Option<&str>, limit: usize) -> Vec<Def> {
        let mut defs = self.query_defs(
            "SELECT path, name, kind, start_line, end_line FROM defs WHERE name = ?1 LIMIT ?2",
            params![name, limit as i64],
        );
        if let Some(near) = near {
            defs.sort_by_key(|d| d.path != near);
        }
        defs
    }

    /// The innermost definition whose body contains `line`.
    pub fn enclosing(&self, path: &str, line: u32) -> Option<Def> {
        self.query_defs(
            "SELECT path, name, kind, start_line, end_line FROM defs \
             WHERE path = ?1 AND start_line <= ?2 AND end_line >= ?2 \
             ORDER BY (end_line - start_line) ASC LIMIT 1",
            params![path, line],
        )
        .into_iter()
        .next()
    }

    /// The definitions that mention `name` — the reverse edge, and the reason
    /// this file exists.
    ///
    /// A reference sitting inside nested declarations belongs to the innermost
    /// one; SQLite's bare-column rule hands back the row matching `MIN`.
    /// Self-references are excluded, or every symbol would appear to call itself.
    pub fn callers_of(&self, name: &str, limit: usize) -> Vec<Def> {
        let mut callers = self.query_defs(
            "SELECT d.path, d.name, d.kind, d.start_line, d.end_line, \
                    MIN(d.end_line - d.start_line) \
             FROM refs r \
             JOIN defs d ON d.path = r.path AND r.line >= d.start_line AND r.line <= d.end_line \
             WHERE r.name = ?1 AND d.name != ?1 \
             GROUP BY r.rowid \
             LIMIT ?2",
            params![name, limit as i64],
        );
        // One caller referencing a symbol five times is still one caller.
        callers.sort_by(|a, b| {
            (&a.path, a.start_line, &a.name).cmp(&(&b.path, b.start_line, &b.name))
        });
        callers
            .dedup_by(|a, b| a.path == b.path && a.start_line == b.start_line && a.name == b.name);
        callers.sort_by_key(|d| d.span());
        callers
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let one = |sql: &str| -> usize {
            self.conn
                .query_row(sql, [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as usize
        };
        (
            one("SELECT COUNT(*) FROM files"),
            one("SELECT COUNT(*) FROM defs"),
            one("SELECT COUNT(*) FROM refs"),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.counts().1 == 0
    }

    fn query_defs(&self, sql: &str, args: impl rusqlite::Params) -> Vec<Def> {
        let Ok(mut stmt) = self.conn.prepare(sql) else {
            return Vec::new();
        };
        let rows = stmt.query_map(args, |r| {
            Ok(Def {
                path: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                start_line: r.get(3)?,
                end_line: r.get(4)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-graph-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn indexed(root: &Path) -> Graph {
        let mut g = Graph::open(root).unwrap();
        g.index(root, &|_| {}).unwrap();
        g
    }

    const LIB: &str = "\
pub fn validate_token(t: &str) -> bool {
    !t.is_empty()
}
";

    const CALLER: &str = "\
pub fn login(token: &str) -> bool {
    validate_token(token)
}

pub fn refresh(token: &str) -> bool {
    validate_token(token)
}
";

    /// The question the regex index could never answer.
    #[test]
    fn callers_are_found_across_files() {
        let root = project("callers");
        write(&root, "src/lib.rs", LIB);
        write(&root, "src/api.rs", CALLER);
        let g = indexed(&root);

        let callers = g.callers_of("validate_token", 10);
        let names: Vec<&str> = callers.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"login"), "got {names:?}");
        assert!(names.contains(&"refresh"), "got {names:?}");
        assert!(
            callers.iter().all(|d| d.path == "src/api.rs"),
            "callers should be located where they are, not where the symbol is"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symbol_does_not_count_as_its_own_caller() {
        let root = project("self");
        write(
            &root,
            "src/a.rs",
            "pub fn recurse(n: u32) { recurse(n - 1) }\n",
        );
        let g = indexed(&root);
        assert!(
            g.callers_of("recurse", 10).is_empty(),
            "self-reference is not a blast radius"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_caller_referencing_a_symbol_twice_is_still_one_caller() {
        let root = project("dedup");
        write(
            &root,
            "src/a.rs",
            "fn target() {}\nfn user() { target(); target(); target(); }\n",
        );
        let g = indexed(&root);
        assert_eq!(g.callers_of("target", 10).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_innermost_definition_owns_a_reference() {
        let root = project("innermost");
        write(
            &root,
            "src/a.rs",
            "mod outer {\n    pub fn inner() { helper(); }\n}\nfn helper() {}\n",
        );
        let g = indexed(&root);
        let callers = g.callers_of("helper", 10);
        assert_eq!(callers[0].name, "inner", "not the enclosing module");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enclosing_finds_the_symbol_a_line_sits_in() {
        let root = project("enclosing");
        write(&root, "src/a.rs", CALLER);
        let g = indexed(&root);
        assert_eq!(
            g.enclosing("src/a.rs", 2).map(|d| d.name),
            Some("login".into())
        );
        assert_eq!(
            g.enclosing("src/a.rs", 6).map(|d| d.name),
            Some("refresh".into())
        );
        assert!(g.enclosing("src/a.rs", 999).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reindexing_skips_files_that_have_not_moved() {
        let root = project("incremental");
        write(&root, "src/a.rs", LIB);
        let mut g = Graph::open(&root).unwrap();

        let first = g.index(&root, &|_| {}).unwrap();
        assert_eq!(first.files_indexed, 1);

        let second = g.index(&root, &|_| {}).unwrap();
        assert_eq!(second.files_indexed, 0, "nothing changed");
        assert_eq!(second.files_unchanged, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_edited_file_is_refreshed_rather_than_duplicated() {
        let root = project("refresh");
        write(&root, "src/a.rs", "pub fn before() {}\n");
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&root, "src/a.rs", "pub fn after() {}\n");
        g.index(&root, &|_| {}).unwrap();

        assert!(
            g.definitions_of("before", None, 10).is_empty(),
            "stale symbol survived"
        );
        assert_eq!(g.definitions_of("after", None, 10).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Why the graph is refreshed before every review rather than on demand.
    ///
    /// A stale graph does not merely miss new code — its line ranges point at
    /// whatever now occupies those lines, and `source` reads the working tree,
    /// so the model is handed unrelated code labelled as the enclosing symbol.
    /// Confidently wrong context is worse than none.
    #[test]
    fn a_stale_graph_reports_the_wrong_lines_until_reindexed() {
        let root = project("stale");
        write(
            &root,
            "src/a.rs",
            "pub fn target() {\n    let secret = 1;\n}\n",
        );
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();

        let before = g.definitions_of("target", None, 1).remove(0);
        assert!(before.source(&root, 100).unwrap().contains("let secret"));

        // Someone adds imports at the top — utterly ordinary.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &root,
            "src/a.rs",
            "// added\n// added\n// added\n// added\n// added\npub fn target() {\n    let secret = 1;\n}\n",
        );

        let stale = g.definitions_of("target", None, 1).remove(0);
        assert!(
            !stale.source(&root, 100).unwrap().contains("let secret"),
            "this is the failure mode being guarded against"
        );

        // Re-indexing is what makes it right again, and is cheap enough to do
        // before every review.
        g.index(&root, &|_| {}).unwrap();
        let fresh = g.definitions_of("target", None, 1).remove(0);
        assert_eq!(fresh.start_line, 6);
        assert!(fresh.source(&root, 100).unwrap().contains("let secret"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_file_drops_out_of_the_graph() {
        let root = project("deleted");
        write(&root, "src/gone.rs", "pub fn vanishing() {}\n");
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();
        assert_eq!(g.definitions_of("vanishing", None, 10).len(), 1);

        std::fs::remove_file(root.join("src/gone.rs")).unwrap();
        let stats = g.index(&root, &|_| {}).unwrap();

        assert_eq!(stats.files_removed, 1);
        assert!(
            g.definitions_of("vanishing", None, 10).is_empty(),
            "a deleted file must stop answering queries"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_local_definition_is_preferred_when_a_name_is_ambiguous() {
        let root = project("ambiguous");
        write(&root, "src/a.rs", "pub struct Config { a: u32 }\n");
        write(&root, "src/b.rs", "pub struct Config { b: u32 }\n");
        let g = indexed(&root);

        let defs = g.definitions_of("Config", Some("src/b.rs"), 10);
        assert_eq!(defs.len(), 2, "both are real and both are kept");
        assert_eq!(defs[0].path, "src/b.rs", "the local one comes first");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn source_is_read_from_the_working_tree_not_the_database() {
        let root = project("source");
        write(&root, "src/a.rs", "pub fn f() {\n    let x = 1;\n}\n");
        let g = indexed(&root);
        let def = g.definitions_of("f", None, 1).remove(0);

        assert!(def.source(&root, 100).unwrap().contains("let x = 1;"));

        // Edit without reindexing: the body must reflect the file, since that is
        // what the reviewer is actually looking at.
        write(&root, "src/a.rs", "pub fn f() {\n    let x = 99;\n}\n");
        assert!(def.source(&root, 100).unwrap().contains("let x = 99;"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_huge_definition_is_capped() {
        let root = project("cap");
        let body = format!("pub fn big() {{\n{}}}\n", "    let x = 1;\n".repeat(500));
        write(&root, "src/a.rs", &body);
        let g = indexed(&root);
        let def = g.definitions_of("big", None, 1).remove(0);
        assert_eq!(def.source(&root, 10).unwrap().lines().count(), 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignored_directories_are_not_walked() {
        let root = project("ignored");
        write(&root, "src/real.rs", "pub fn real() {}\n");
        write(&root, "node_modules/dep/index.js", "function fake() {}\n");
        write(&root, "target/debug/gen.rs", "pub fn generated() {}\n");
        let g = indexed(&root);

        assert_eq!(g.definitions_of("real", None, 10).len(), 1);
        assert!(g.definitions_of("fake", None, 10).is_empty());
        assert!(g.definitions_of("generated", None, 10).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_schema_from_another_version_is_rebuilt_rather_than_queried() {
        let root = project("schema");
        write(&root, "src/a.rs", LIB);
        {
            let mut g = Graph::open(&root).unwrap();
            g.index(&root, &|_| {}).unwrap();
        }
        // Simulate an older build's schema marker.
        {
            let conn = Connection::open(Graph::path(&root)).unwrap();
            conn.execute_batch("PRAGMA user_version = 999;").unwrap();
        }
        let g = Graph::open(&root).unwrap();
        assert!(
            g.is_empty(),
            "a shape we do not understand must be discarded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_project_is_not_an_error() {
        let root = project("empty");
        let g = indexed(&root);
        assert_eq!(g.counts(), (0, 0, 0));
        assert!(g.callers_of("anything", 10).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
