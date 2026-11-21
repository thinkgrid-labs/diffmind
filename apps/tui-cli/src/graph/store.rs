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
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::extract::{self, Lang};

/// Bumped when the schema changes. A mismatch rebuilds from scratch rather than
/// querying a shape this build does not understand.
///
/// 2: files gained `ctime`, `size`, `hash` and `indexed_at` for content-based
///    freshness.
const SCHEMA_VERSION: i64 = 2;

/// A file whose mtime is this close to (or after) the moment it was last
/// indexed is "racily clean" in git's sense: it could have been written again
/// within the filesystem's timestamp granularity — one or two seconds on FAT,
/// HFS+ and many network filesystems — without its mtime moving. Such a file
/// is re-hashed rather than trusted. Older files take the mtime fast path, so
/// a review never hashes the whole repository.
const RACY_WINDOW_SECS: f64 = 2.0;

/// Wait this long for another process's index transaction instead of failing
/// immediately with "database is locked".
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Files larger than this are not parsed. Hand-written source this size is
/// vanishingly rare; bundles and generated files are common and would cost
/// seconds of tree-sitter time on every change for symbols nobody wants.
const MAX_INDEX_FILE_BYTES: u64 = 4 * 1024 * 1024;

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
        let text = super::read_repo_file(project_root, &self.path)?;
        let lines: Vec<&str> = text.lines().collect();
        let start = (self.start_line as usize).saturating_sub(1);
        if start >= lines.len() {
            return None;
        }
        let end = (self.end_line as usize).min(lines.len());
        let end = end.min(start + max_lines);
        Some(lines[start..end].join("\n"))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_unchanged: usize,
    pub files_removed: usize,
    /// Files read and hashed because the metadata fast path could not vouch
    /// for them. Reported so the cost of content checking stays visible.
    pub files_hashed: usize,
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
    ///
    /// A database SQLite reports as corrupt, or as not a database at all, is
    /// deleted and rebuilt. It is a derived cache of the working tree, so
    /// nothing is lost — and leaving it would silently disable code context on
    /// every future review, since a failed open is (deliberately) not fatal.
    pub fn open(project_root: &Path) -> Result<Graph> {
        let path = Self::path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match Self::open_at(&path) {
            Ok(g) => Ok(g),
            Err(e) if is_corruption(&e) => {
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let mut p = path.clone().into_os_string();
                    p.push(suffix);
                    let _ = std::fs::remove_file(PathBuf::from(p));
                }
                Self::open_at(&path)
                    .with_context(|| format!("could not rebuild {}", path.display()))
            }
            Err(e) => Err(e).with_context(|| format!("could not open {}", path.display())),
        }
    }

    fn open_at(path: &Path) -> rusqlite::Result<Graph> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;

        // Per-connection, takes no lock.
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == SCHEMA_VERSION {
            // Already current: open read-only in effect. Running the schema
            // batch anyway needs the write lock, so every open — including a
            // review's — used to queue behind any index in progress and could
            // fail with "database is locked" once the busy timeout ran out.
            return Ok(Graph { conn });
        }
        // WAL lets reviews read while an index writes. It cannot be switched
        // inside a transaction, and a concurrent opener may hold the database
        // briefly; it is an optimisation, so a failure here is not fatal.
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");

        // Migrate under the write lock and re-check the version inside it:
        // two first-time openers must not both see "version 0" and have the
        // second drop the tables the first just created and began filling.
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let migrate = (|| -> rusqlite::Result<()> {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version == SCHEMA_VERSION {
                return Ok(());
            }
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS refs;
                 DROP TABLE IF EXISTS defs;
                 DROP TABLE IF EXISTS files;
                 CREATE TABLE files (
                     path       TEXT PRIMARY KEY,
                     mtime      REAL NOT NULL,
                     ctime      REAL NOT NULL,
                     size       INTEGER NOT NULL,
                     hash       TEXT NOT NULL,
                     indexed_at REAL NOT NULL
                 );
                 CREATE TABLE defs (
                     path       TEXT NOT NULL,
                     name       TEXT NOT NULL,
                     kind       TEXT NOT NULL,
                     start_line INTEGER NOT NULL,
                     end_line   INTEGER NOT NULL
                 );
                 CREATE TABLE refs (
                     path TEXT NOT NULL,
                     name TEXT NOT NULL,
                     line INTEGER NOT NULL
                 );
                 CREATE INDEX defs_name ON defs(name);
                 CREATE INDEX defs_path ON defs(path);
                 CREATE INDEX refs_name ON refs(name);
                 CREATE INDEX refs_path ON refs(path);
                 PRAGMA user_version = {SCHEMA_VERSION};"
            ))
        })();
        match migrate {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }

        Ok(Graph { conn })
    }

    /// Walk the project and bring the graph up to date, reparsing only files
    /// whose mtime moved.
    pub fn index(&mut self, project_root: &Path, on_file: &dyn Fn(usize)) -> Result<IndexStats> {
        let mut stats = IndexStats::default();
        let mut seen: HashSet<String> = HashSet::new();
        // Taken once, before any file is read: a file modified after this
        // instant is newer than its index row by construction.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        // IMMEDIATE takes the write lock up front. A deferred transaction reads
        // first and upgrades later, and in WAL mode that upgrade fails with
        // SQLITE_BUSY_SNAPSHOT whenever another indexer committed in between —
        // an error no busy timeout can wait out.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

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

            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
            let ctime = change_time(&meta);

            seen.insert(path.clone());

            let known: Option<(f64, f64, i64, String, f64)> = tx
                .query_row(
                    "SELECT mtime, ctime, size, hash, indexed_at FROM files WHERE path = ?1",
                    [&path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .ok();

            // Fast path: same mtime, same inode change time, same size, and
            // old enough that a second write could not have hidden inside the
            // timestamp's resolution. The change time is what catches a file
            // rewritten and then given back an old mtime (`cp -p`, `rsync -t`,
            // `touch -r`): those tools can set mtime but not ctime. Windows
            // has no ctime; there only the mtime, size and racy window apply.
            if let Some((known_mtime, known_ctime, known_size, _, indexed_at)) = &known
                && *known_mtime == mtime
                && *known_ctime == ctime
                && *known_size == size
                && mtime + RACY_WINDOW_SECS < *indexed_at
                && ctime + RACY_WINDOW_SECS < *indexed_at
            {
                stats.files_unchanged += 1;
                continue;
            }

            // Too big to be hand-written source worth parsing (a bundled or
            // generated file); recorded with no symbols rather than read.
            if meta.len() > MAX_INDEX_FILE_BYTES {
                let marker = format!("too-large:{size}");
                tx.execute("DELETE FROM defs WHERE path = ?1", [&path])?;
                tx.execute("DELETE FROM refs WHERE path = ?1", [&path])?;
                tx.execute(
                    "INSERT INTO files (path, mtime, ctime, size, hash, indexed_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime, \
                     ctime = excluded.ctime, size = excluded.size, hash = excluded.hash, \
                     indexed_at = excluded.indexed_at",
                    params![path, mtime, ctime, size, marker, now],
                )?;
                stats.files_unchanged += 1;
                continue;
            }

            // Otherwise the content decides. mtime alone was the old rule, and
            // it misses an edit that lands in the same timestamp tick, a file
            // restored with its original mtime (`cp -p`, `rsync -t`, `tar x`),
            // and a clock that steps backwards.
            let bytes = std::fs::read(entry.path()).unwrap_or_default();
            let hash = format!("{:x}", Sha256::digest(&bytes));
            stats.files_hashed += 1;
            if let Some((_, _, _, known_hash, _)) = &known
                && *known_hash == hash
            {
                tx.execute(
                    "UPDATE files SET mtime = ?2, ctime = ?3, size = ?4, indexed_at = ?5 \
                     WHERE path = ?1",
                    params![path, mtime, ctime, size, now],
                )?;
                stats.files_unchanged += 1;
                continue;
            }

            // A file that cannot be read as text or parsed must not keep the
            // rows of the version that could: those line ranges now point at
            // whatever the file holds instead. It is recorded with no symbols.
            let extracted = String::from_utf8(bytes)
                .ok()
                .and_then(|source| extract::extract(lang, &source))
                .unwrap_or_default();

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
                "INSERT INTO files (path, mtime, ctime, size, hash, indexed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime, \
                 ctime = excluded.ctime, size = excluded.size, hash = excluded.hash, \
                 indexed_at = excluded.indexed_at",
                params![path, mtime, ctime, size, hash, now],
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

    /// Every definition in `path` overlapping `start..=end`, innermost first.
    ///
    /// For a caller that needs the enclosing symbol of *many* lines. Asking
    /// [`Self::enclosing`] per line is one query per line, and a unit spanning
    /// several hundred lines then costs several hundred queries before any
    /// inference starts. One query, resolved in memory, answers the same
    /// question — see `graph::link`.
    pub fn declarations_overlapping(&self, path: &str, start: u32, end: u32) -> Vec<Def> {
        self.query_defs(
            "SELECT path, name, kind, start_line, end_line FROM defs \
             WHERE path = ?1 AND start_line <= ?3 AND end_line >= ?2 \
             ORDER BY (end_line - start_line), start_line, name",
            params![path, start, end],
        )
    }

    /// Every definition whose name is one of `names`.
    ///
    /// Ordered by `(name, path, start_line)`, so a caller picking one definition
    /// per name gets the same answer on every run. Batched rather than one query
    /// per name: `rag` derives candidate names from every word in the diff, which
    /// is thousands of lookups on a large branch — and it does it per review
    /// unit.
    ///
    /// Chunked so a diff with more distinct words than SQLite's bound-parameter
    /// limit still works; in practice this is a single query.
    pub fn definitions_of_names(&self, names: &[String]) -> Vec<Def> {
        /// Comfortably below SQLite's `SQLITE_MAX_VARIABLE_NUMBER`, whose
        /// historical value was 999.
        const CHUNK: usize = 400;

        let mut out = Vec::new();
        for batch in names.chunks(CHUNK) {
            let placeholders = vec!["?"; batch.len()].join(",");
            out.extend(self.query_defs(
                &format!(
                    "SELECT path, name, kind, start_line, end_line FROM defs \
                     WHERE name IN ({placeholders}) ORDER BY name, path, start_line"
                ),
                rusqlite::params_from_iter(batch.iter()),
            ));
        }
        out
    }

    /// The definitions that mention `name` — the reverse edge, and the reason
    /// this file exists.
    ///
    /// `limit` bounds **callers**, not references. Deduplicating in Rust after a
    /// SQL `LIMIT` looked equivalent and was not: the limit fell on reference
    /// rows, so one function calling a symbol three times consumed the whole
    /// budget and every other caller — the ones a blast radius exists to
    /// surface — was silently dropped. `rag.rs` asks for three.
    ///
    /// Two levels, therefore. The inner query resolves each reference to the
    /// innermost declaration containing it (nested declarations both match the
    /// join; SQLite's bare-column rule hands back the row matching `MIN`).
    /// The outer collapses those to distinct callers before the limit applies.
    ///
    /// Ordered smallest-span first, then by location, so the most specific
    /// caller leads and the result does not vary between identical runs — an
    /// unordered `LIMIT` is whatever the query planner felt like returning.
    /// Self-references are excluded, or every symbol would appear to call itself.
    pub fn callers_of(&self, name: &str, limit: usize) -> Vec<Def> {
        self.query_defs(
            "SELECT path, name, kind, start_line, end_line FROM ( \
                 SELECT d.path AS path, d.name AS name, d.kind AS kind, \
                        d.start_line AS start_line, d.end_line AS end_line, \
                        MIN(d.end_line - d.start_line) \
                 FROM refs r \
                 JOIN defs d ON d.path = r.path \
                            AND r.line >= d.start_line AND r.line <= d.end_line \
                 WHERE r.name = ?1 AND d.name != ?1 \
                 GROUP BY r.rowid \
             ) \
             GROUP BY path, name, kind, start_line, end_line \
             ORDER BY (end_line - start_line), path, start_line, name \
             LIMIT ?2",
            params![name, limit as i64],
        )
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

    /// How many definitions share `name` across the repository. Used to tell
    /// a resolved relationship from a name match that could be any of several.
    pub fn definition_count(&self, name: &str) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM defs WHERE name = ?1", [name], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| usize::try_from(n).unwrap_or(0))
            .unwrap_or(0)
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

/// The inode change time in seconds, or 0 where the platform has none.
#[cfg(unix)]
fn change_time(meta: &std::fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    meta.ctime() as f64 + meta.ctime_nsec() as f64 / 1e9
}

#[cfg(not(unix))]
fn change_time(_meta: &std::fs::Metadata) -> f64 {
    0.0
}

/// SQLite's way of saying the file is not a usable database.
fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            )
    )
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

    /// Definitions of a single name, for tests that only care whether the graph
    /// knows a symbol. Production resolves names in batches — a query per name
    /// is what made context assembly scale with the size of the diff.
    fn defs_of(g: &Graph, name: &str) -> Vec<Def> {
        g.definitions_of_names(&[name.to_string()])
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

    /// The limit counts callers, not references.
    ///
    /// It used to bound the reference *rows* and deduplicate afterwards, so a
    /// function calling the symbol several times spent the whole budget on its
    /// own. Three callers of six calls each, asking for three, returned one —
    /// and `rag.rs` asks for three. The blast radius quietly collapsed to the
    /// first caller the query planner happened to reach.
    #[test]
    fn a_chatty_caller_does_not_crowd_out_the_others() {
        let root = project("crowding");
        write(&root, "src/lib.rs", "fn target() {}\n");
        for name in ["a", "m", "z"] {
            let calls = "    target();\n".repeat(6);
            write(
                &root,
                &format!("src/{name}.rs"),
                &format!("fn caller_{name}() {{\n{calls}}}\n"),
            );
        }
        let g = indexed(&root);

        let distinct = |k: usize| -> Vec<String> {
            g.callers_of("target", k)
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert_eq!(
            distinct(3),
            ["caller_a", "caller_m", "caller_z"],
            "three callers exist and three were asked for"
        );
        assert_eq!(distinct(2).len(), 2, "a smaller limit still yields callers");
        assert_eq!(distinct(1).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unordered `LIMIT` returns whatever the planner chose, so the same
    /// repository could yield different review context from run to run — and a
    /// review that is not reproducible cannot be a gate.
    #[test]
    fn callers_come_back_in_a_stable_most_specific_first_order() {
        let root = project("ordering");
        write(&root, "src/lib.rs", "fn target() {}\n");
        write(
            &root,
            "src/big.rs",
            &format!(
                "fn big() {{\n    target();\n{}}}\n",
                "    let x = 1;\n".repeat(20)
            ),
        );
        write(&root, "src/small.rs", "fn small() { target(); }\n");
        let g = indexed(&root);

        let names = |n: usize| -> Vec<String> {
            g.callers_of("target", n)
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert_eq!(names(10), ["small", "big"], "tightest scope leads");
        assert_eq!(names(10), names(10), "repeated queries must agree");
        assert_eq!(
            names(1),
            ["small"],
            "a limit must keep the most specific caller, not an arbitrary one"
        );
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

        assert!(defs_of(&g, "before").is_empty(), "stale symbol survived");
        assert_eq!(defs_of(&g, "after").len(), 1);
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

        let before = defs_of(&g, "target").remove(0);
        assert!(before.source(&root, 100).unwrap().contains("let secret"));

        // Someone adds imports at the top — utterly ordinary.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &root,
            "src/a.rs",
            "// added\n// added\n// added\n// added\n// added\npub fn target() {\n    let secret = 1;\n}\n",
        );

        let stale = defs_of(&g, "target").remove(0);
        assert!(
            !stale.source(&root, 100).unwrap().contains("let secret"),
            "this is the failure mode being guarded against"
        );

        // Re-indexing is what makes it right again, and is cheap enough to do
        // before every review.
        g.index(&root, &|_| {}).unwrap();
        let fresh = defs_of(&g, "target").remove(0);
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
        assert_eq!(defs_of(&g, "vanishing").len(), 1);

        std::fs::remove_file(root.join("src/gone.rs")).unwrap();
        let stats = g.index(&root, &|_| {}).unwrap();

        assert_eq!(stats.files_removed, 1);
        assert!(
            defs_of(&g, "vanishing").is_empty(),
            "a deleted file must stop answering queries"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An ambiguous name keeps every candidate, in a fixed order, so the caller
    /// choosing between them (`rag`, which prefers a definition in the file the
    /// reference was seen in) gets the same answer on every run.
    #[test]
    fn an_ambiguous_name_returns_every_candidate_in_a_stable_order() {
        let root = project("ambiguous");
        write(&root, "src/b.rs", "pub struct Config { b: u32 }\n");
        write(&root, "src/a.rs", "pub struct Config { a: u32 }\n");
        let g = indexed(&root);

        let defs = defs_of(&g, "Config");
        assert_eq!(defs.len(), 2, "both are real and both are kept");
        let paths: Vec<&str> = defs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            ["src/a.rs", "src/b.rs"],
            "ordered by path regardless of which file was indexed first"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Several names in one round trip, which is how `rag` resolves the words it
    /// harvests from a diff. Names the graph does not know simply do not appear.
    #[test]
    fn many_names_resolve_in_one_query() {
        let root = project("batch");
        write(
            &root,
            "src/a.rs",
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
        );
        let g = indexed(&root);

        let names: Vec<String> = ["gamma", "alpha", "nonexistent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let resolved = g.definitions_of_names(&names);
        let found: Vec<&str> = resolved.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            found,
            ["alpha", "gamma"],
            "ordered by name, unknowns absent"
        );
        assert!(g.definitions_of_names(&[]).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The query the per-line `enclosing` loop was replaced with.
    #[test]
    fn declarations_overlapping_a_span_come_back_innermost_first() {
        let root = project("overlapping");
        write(
            &root,
            "src/a.rs",
            "mod outer {\n    pub fn inner() {\n        let x = 1;\n    }\n}\nfn after() {}\n",
        );
        let g = indexed(&root);

        let overlapping = g.declarations_overlapping("src/a.rs", 2, 3);
        let names: Vec<&str> = overlapping.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["inner", "outer"],
            "both contain the span; the tighter one leads"
        );

        // The same answer `enclosing` gives, for every line in the span.
        for line in 2..=3 {
            assert_eq!(
                g.enclosing("src/a.rs", line).map(|d| d.name),
                Some("inner".into()),
                "line {line}"
            );
        }
        assert!(
            g.declarations_overlapping("src/a.rs", 90, 99).is_empty(),
            "a span past the end of the file declares nothing"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn source_is_read_from_the_working_tree_not_the_database() {
        let root = project("source");
        write(&root, "src/a.rs", "pub fn f() {\n    let x = 1;\n}\n");
        let g = indexed(&root);
        let def = defs_of(&g, "f").remove(0);

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
        let def = defs_of(&g, "big").remove(0);
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

        assert_eq!(defs_of(&g, "real").len(), 1);
        assert!(defs_of(&g, "fake").is_empty());
        assert!(defs_of(&g, "generated").is_empty());
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

    /// Set a file's mtime explicitly, so a test can make two different
    /// contents share one timestamp — what a coarse filesystem, `cp -p`,
    /// `rsync -t` or an edit within one tick does.
    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    /// The defect: freshness was mtime alone, so an edit that did not move the
    /// mtime left the old symbols — and their line ranges — in the graph.
    /// Here the edit even restores an hour-old mtime; the inode change time
    /// still moves.
    #[cfg(unix)]
    #[test]
    fn an_edit_that_keeps_the_same_mtime_and_size_is_still_detected() {
        let root = project("same-mtime");
        let file = root.join("src/a.rs");
        let fixed = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);

        write(&root, "src/a.rs", "pub fn alpha() {}\n");
        set_mtime(&file, fixed);
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();
        assert_eq!(defs_of(&g, "alpha").len(), 1);

        // Same length, same mtime, different symbol.
        write(&root, "src/a.rs", "pub fn bravo() {}\n");
        set_mtime(&file, fixed);
        let stats = g.index(&root, &|_| {}).unwrap();
        assert_eq!(stats.files_indexed, 1, "the content changed");
        assert!(defs_of(&g, "alpha").is_empty(), "stale symbol survived");
        assert_eq!(defs_of(&g, "bravo").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The platform-independent half: an edit landing in the same timestamp
    /// tick as the previous index (mtime within the racy window) is re-hashed.
    #[test]
    fn an_edit_within_one_timestamp_tick_is_detected() {
        let root = project("racy");
        let file = root.join("src/a.rs");
        let tick = std::time::SystemTime::now() - std::time::Duration::from_millis(500);
        write(&root, "src/a.rs", "pub fn alpha() {}\n");
        set_mtime(&file, tick);
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();

        write(&root, "src/a.rs", "pub fn bravo() {}\n");
        set_mtime(&file, tick);
        let stats = g.index(&root, &|_| {}).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(defs_of(&g, "bravo").len(), 1);
        assert!(defs_of(&g, "alpha").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Re-hashing is reserved for files that could be racy; an old, untouched
    /// file takes the metadata fast path, so a large repository is not read
    /// and hashed on every review.
    #[test]
    fn an_old_unchanged_file_is_not_reread() {
        let root = project("fast-path");
        let file = root.join("src/a.rs");
        write(&root, "src/a.rs", LIB);
        set_mtime(
            &file,
            std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();
        // The first index ran within the racy window of the file's ctime, so
        // wait it out once; after that the fast path holds.
        std::thread::sleep(std::time::Duration::from_secs_f64(RACY_WINDOW_SECS + 0.2));
        g.index(&root, &|_| {}).unwrap();

        let stats = g.index(&root, &|_| {}).unwrap();
        assert_eq!(stats.files_unchanged, 1);
        assert_eq!(stats.files_hashed, 0, "an untouched old file was re-read");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rapid_consecutive_edits_all_land() {
        let root = project("rapid");
        let mut g = Graph::open(&root).unwrap();
        for i in 0..5 {
            write(&root, "src/a.rs", &format!("pub fn version_{i}() {{}}\n"));
            g.index(&root, &|_| {}).unwrap();
            assert_eq!(defs_of(&g, &format!("version_{i}")).len(), 1, "edit {i}");
            if i > 0 {
                assert!(defs_of(&g, &format!("version_{}", i - 1)).is_empty());
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_renamed_file_moves_its_symbols() {
        let root = project("rename");
        write(&root, "src/old_name.rs", "pub fn moved() {}\n");
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();
        std::fs::rename(root.join("src/old_name.rs"), root.join("src/new_name.rs")).unwrap();
        let stats = g.index(&root, &|_| {}).unwrap();
        assert_eq!(stats.files_removed, 1);
        let d = defs_of(&g, "moved");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "src/new_name.rs");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file that stops parsing (or stops being UTF-8) must not keep the
    /// previous version's rows — their line ranges now point at other text.
    #[test]
    fn a_file_that_becomes_unparseable_drops_its_old_symbols() {
        let root = project("unparseable");
        write(&root, "src/a.rs", "pub fn was_here() {}\n");
        let mut g = Graph::open(&root).unwrap();
        g.index(&root, &|_| {}).unwrap();
        std::fs::write(root.join("src/a.rs"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
        g.index(&root, &|_| {}).unwrap();
        assert!(defs_of(&g, "was_here").is_empty());

        // Malformed but valid UTF-8 never panics the indexer.
        write(
            &root,
            "src/b.rs",
            "pub fn ((( {{{ ]]] impl <<<\n\u{0}\u{FEFF}",
        );
        g.index(&root, &|_| {}).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unicode_paths_and_identifiers_round_trip() {
        let root = project("unicode");
        write(
            &root,
            "src/módulo_ñ.py",
            "def función_ünïcode():\n    return 1\n",
        );
        let g = indexed(&root);
        let d = defs_of(&g, "función_ünïcode");
        assert_eq!(d.len(), 1, "{:?}", g.counts());
        assert_eq!(d[0].path, "src/módulo_ñ.py");
        assert!(d[0].source(&root, 10).unwrap().contains("return 1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_are_not_indexed() {
        let root = project("symlink-index");
        let outside = root
            .parent()
            .unwrap()
            .join(format!("diffmind-outside-{}.rs", std::process::id()));
        std::fs::write(&outside, "pub fn outside_repo() {}\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("src/link.rs")).unwrap();
        let g = indexed(&root);
        assert!(defs_of(&g, "outside_repo").is_empty());
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A corrupt database used to make every `Graph::open` fail, and because a
    /// failed open is not fatal, every later review silently ran without code
    /// context. It is now discarded and rebuilt.
    #[test]
    fn a_corrupt_database_is_rebuilt() {
        let root = project("corrupt-db");
        write(&root, "src/a.rs", LIB);
        indexed(&root);
        std::fs::write(
            Graph::path(&root),
            b"this is not an sqlite database at all, just junk bytes......",
        )
        .unwrap();
        let _ = std::fs::remove_file(root.join(".diffmind/graph.db-wal"));
        let _ = std::fs::remove_file(root.join(".diffmind/graph.db-shm"));
        let mut g = Graph::open(&root).expect("rebuilt rather than failing");
        g.index(&root, &|_| {}).unwrap();
        assert_eq!(defs_of(&g, "validate_token").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two processes indexing at once (an editor hook and a review) must both
    /// succeed: the second waits for the first's transaction rather than
    /// failing with "database is locked".
    #[test]
    fn concurrent_indexing_waits_instead_of_failing() {
        let root = project("concurrent-index");
        for i in 0..12 {
            write(
                &root,
                &format!("src/f{i}.rs"),
                &format!("pub fn f{i}() {{}}\n"),
            );
        }
        Graph::open(&root).unwrap();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut g = Graph::open(&root)?;
                    g.index(&root, &|_| {}).map(|_| ())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("every concurrent index succeeds");
        }
        let g = Graph::open(&root).unwrap();
        assert_eq!(g.counts().0, 12);
        assert_eq!(defs_of(&g, "f7").len(), 1, "no duplicated or lost rows");
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
