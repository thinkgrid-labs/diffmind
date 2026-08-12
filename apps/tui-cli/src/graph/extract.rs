//! Tree-sitter symbol extraction: source text in, definitions and references out.
//!
//! This replaces a pile of regexes that could only see `pub`/`export` symbols
//! and had no concept of a reference at all. The regex index could answer "where
//! is `validateToken` defined"; it could never answer "what calls it", which is
//! the question a reviewer actually has about a changed function.
//!
//! ## Tolerant at runtime, strict in tests
//!
//! Each pattern is compiled on its own and a pattern that fails to compile is
//! skipped rather than taking the whole language down with it. Grammar crates
//! rename nodes between versions, and losing one kind of definition is far
//! better than losing every symbol in the repository the day a dependency is
//! bumped. `every_pattern_compiles` then fails loudly in CI, so the degradation
//! is never silent.

// Consumed by the graph store, which lands next. Kept separate so the parsing
// layer could be tested against real grammars before any storage decisions were
// baked in.
#![allow(dead_code)]

use std::path::Path;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
}

impl Lang {
    pub fn for_path(path: &Path) -> Option<Lang> {
        match path.extension()?.to_str()? {
            "rs" => Some(Lang::Rust),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" => Some(Lang::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
            "py" | "pyi" => Some(Lang::Python),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    /// Every language this build can parse.
    pub fn all() -> [Lang; 5] {
        [
            Lang::Rust,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Python,
        ]
    }
}

/// A symbol declared in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub kind: &'static str,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
}

/// A mention of a name — a call, a constructor, a type position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub line: u32,
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
}

/// A definition pattern. The query must capture the whole declaration as `@def`
/// and its name as `@name`; the range comes from `@def` so that "which symbol
/// encloses line N" has a real body to test against.
struct DefPattern {
    kind: &'static str,
    query: &'static str,
}

const fn def(kind: &'static str, query: &'static str) -> DefPattern {
    DefPattern { kind, query }
}

const RUST_DEFS: &[DefPattern] = &[
    def("function", "(function_item name: (identifier) @name) @def"),
    def("struct", "(struct_item name: (type_identifier) @name) @def"),
    def("enum", "(enum_item name: (type_identifier) @name) @def"),
    def("trait", "(trait_item name: (type_identifier) @name) @def"),
    def("type", "(type_item name: (type_identifier) @name) @def"),
    def("const", "(const_item name: (identifier) @name) @def"),
    def("static", "(static_item name: (identifier) @name) @def"),
    def("macro", "(macro_definition name: (identifier) @name) @def"),
    def("module", "(mod_item name: (identifier) @name) @def"),
];

const TS_DEFS: &[DefPattern] = &[
    def(
        "function",
        "(function_declaration name: (identifier) @name) @def",
    ),
    def(
        "class",
        "(class_declaration name: (type_identifier) @name) @def",
    ),
    def(
        "interface",
        "(interface_declaration name: (type_identifier) @name) @def",
    ),
    def(
        "type",
        "(type_alias_declaration name: (type_identifier) @name) @def",
    ),
    def("enum", "(enum_declaration name: (identifier) @name) @def"),
    def(
        "method",
        "(method_definition name: (property_identifier) @name) @def",
    ),
    // `const handler = () => {}` is how most TypeScript functions are actually
    // written; missing it would miss most of a codebase.
    def(
        "function",
        "(variable_declarator name: (identifier) @name value: (arrow_function)) @def",
    ),
    def(
        "function",
        "(variable_declarator name: (identifier) @name value: (function_expression)) @def",
    ),
];

const JS_DEFS: &[DefPattern] = &[
    def(
        "function",
        "(function_declaration name: (identifier) @name) @def",
    ),
    def("class", "(class_declaration name: (identifier) @name) @def"),
    def(
        "method",
        "(method_definition name: (property_identifier) @name) @def",
    ),
    def(
        "function",
        "(variable_declarator name: (identifier) @name value: (arrow_function)) @def",
    ),
    def(
        "function",
        "(variable_declarator name: (identifier) @name value: (function_expression)) @def",
    ),
];

const PY_DEFS: &[DefPattern] = &[
    def(
        "function",
        "(function_definition name: (identifier) @name) @def",
    ),
    def("class", "(class_definition name: (identifier) @name) @def"),
];

fn def_patterns(lang: Lang) -> &'static [DefPattern] {
    match lang {
        Lang::Rust => RUST_DEFS,
        Lang::TypeScript | Lang::Tsx => TS_DEFS,
        Lang::JavaScript => JS_DEFS,
        Lang::Python => PY_DEFS,
    }
}

const RUST_REFS: &[&str] = &[
    "(call_expression function: (identifier) @ref)",
    "(call_expression function: (field_expression field: (field_identifier) @ref))",
    "(call_expression function: (scoped_identifier name: (identifier) @ref))",
    "(macro_invocation macro: (identifier) @ref)",
    "(type_identifier) @ref",
];

const JS_REFS: &[&str] = &[
    "(call_expression function: (identifier) @ref)",
    "(call_expression function: (member_expression property: (property_identifier) @ref))",
    "(new_expression constructor: (identifier) @ref)",
];

/// TypeScript is JavaScript plus type positions — a node the JavaScript grammar
/// does not have at all.
const TS_REFS: &[&str] = &[
    "(call_expression function: (identifier) @ref)",
    "(call_expression function: (member_expression property: (property_identifier) @ref))",
    "(new_expression constructor: (identifier) @ref)",
    "(type_identifier) @ref",
];

const PY_REFS: &[&str] = &[
    "(call function: (identifier) @ref)",
    "(call function: (attribute attribute: (identifier) @ref))",
];

/// Reference patterns. Deliberately narrow: call targets, constructors and type
/// positions. Capturing every bare identifier would make `callers_of` return
/// most of the repository, and a blast radius that includes everything is the
/// same as no blast radius at all.
fn ref_patterns(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => RUST_REFS,
        Lang::TypeScript | Lang::Tsx => TS_REFS,
        Lang::JavaScript => JS_REFS,
        Lang::Python => PY_REFS,
    }
}

/// Parse one file. Returns `None` when the language is unsupported or the
/// parser cannot produce a tree at all; a file with syntax errors still yields
/// whatever tree-sitter could recover, which is the point of using it.
pub fn extract(lang: Lang, source: &str) -> Option<Extracted> {
    let grammar = lang.grammar();
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut out = Extracted::default();

    for pattern in def_patterns(lang) {
        let Ok(query) = Query::new(&grammar, pattern.query) else {
            continue;
        };
        let (Some(name_ix), Some(def_ix)) = (
            query.capture_index_for_name("name"),
            query.capture_index_for_name("def"),
        ) else {
            continue;
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, bytes);
        while let Some(m) = matches.next() {
            let name = capture(m, name_ix).and_then(|n| text(n, bytes));
            let body = capture(m, def_ix);
            if let (Some(name), Some(body)) = (name, body) {
                out.definitions.push(Definition {
                    name,
                    kind: pattern.kind,
                    start_line: line_of(body.start_position().row),
                    end_line: line_of(body.end_position().row),
                });
            }
        }
    }

    for pattern in ref_patterns(lang) {
        let Ok(query) = Query::new(&grammar, pattern) else {
            continue;
        };
        let Some(ref_ix) = query.capture_index_for_name("ref") else {
            continue;
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, bytes);
        while let Some(m) = matches.next() {
            if let Some(node) = capture(m, ref_ix)
                && let Some(name) = text(node, bytes)
            {
                out.references.push(Reference {
                    name,
                    line: line_of(node.start_position().row),
                });
            }
        }
    }

    // A name matched by two overlapping patterns is one symbol, not two.
    out.definitions
        .sort_by(|a, b| (a.start_line, &a.name, a.kind).cmp(&(b.start_line, &b.name, b.kind)));
    out.definitions
        .dedup_by(|a, b| a.name == b.name && a.start_line == b.start_line);

    out.references
        .sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    out.references.dedup();

    Some(out)
}

fn capture<'a>(m: &tree_sitter::QueryMatch<'a, 'a>, index: u32) -> Option<Node<'a>> {
    m.captures.iter().find(|c| c.index == index).map(|c| c.node)
}

fn text(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    node.utf8_text(bytes).ok().map(str::to_string)
}

/// tree-sitter rows are 0-based; every line number diffmind handles is 1-based.
fn line_of(row: usize) -> u32 {
    row as u32 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(defs: &[Definition]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    fn refs(e: &Extracted) -> Vec<&str> {
        e.references.iter().map(|r| r.name.as_str()).collect()
    }

    /// A grammar bump that renames a node would otherwise silently stop
    /// extracting one kind of symbol. Runtime skips the broken pattern; this
    /// makes CI say so.
    #[test]
    fn every_pattern_compiles() {
        for lang in Lang::all() {
            let grammar = lang.grammar();
            for p in def_patterns(lang) {
                assert!(
                    Query::new(&grammar, p.query).is_ok(),
                    "{}: definition pattern failed to compile: {}",
                    lang.name(),
                    p.query
                );
            }
            for p in ref_patterns(lang) {
                assert!(
                    Query::new(&grammar, p).is_ok(),
                    "{}: reference pattern failed to compile: {p}",
                    lang.name()
                );
            }
        }
    }

    #[test]
    fn rust_definitions_and_calls() {
        let src = "\
pub fn validate_token(t: &str) -> bool { true }

struct Session { id: u32 }

pub fn login() {
    let ok = validate_token(\"x\");
    Session { id: 1 };
}
";
        let e = extract(Lang::Rust, src).unwrap();
        assert!(names(&e.definitions).contains(&"validate_token"));
        assert!(names(&e.definitions).contains(&"login"));
        assert!(names(&e.definitions).contains(&"Session"));
        assert!(
            refs(&e).contains(&"validate_token"),
            "the call site is the whole point: {:?}",
            refs(&e)
        );
    }

    #[test]
    fn private_rust_functions_are_found() {
        // The regex indexer only ever saw `pub fn`, so a private helper — the
        // most likely thing to change — was invisible.
        let e = extract(Lang::Rust, "fn helper() {}\n").unwrap();
        assert_eq!(names(&e.definitions), ["helper"]);
    }

    #[test]
    fn a_definition_spans_its_whole_body() {
        let src = "fn outer() {\n    let a = 1;\n    let b = 2;\n}\n";
        let d = &extract(Lang::Rust, src).unwrap().definitions[0];
        assert_eq!((d.start_line, d.end_line), (1, 4));
    }

    #[test]
    fn typescript_arrow_consts_count_as_functions() {
        // Most TypeScript functions are written this way; missing them would
        // miss most of a real codebase.
        let src = "\
export const handler = async (req: Request) => { return check(req); };
export function check(r: Request): boolean { return true; }
export interface Options { a: string }
type Alias = string;
class Service { run() { this.helper(); } helper() {} }
";
        let e = extract(Lang::TypeScript, src).unwrap();
        let n = names(&e.definitions);
        for expected in [
            "handler", "check", "Options", "Alias", "Service", "run", "helper",
        ] {
            assert!(n.contains(&expected), "missing {expected} in {n:?}");
        }
        assert!(refs(&e).contains(&"check"));
        assert!(refs(&e).contains(&"helper"));
    }

    #[test]
    fn tsx_parses_as_tsx_not_typescript() {
        // The TS grammar rejects JSX; using the wrong one silently yields a
        // tree full of errors and almost no symbols.
        let src = "export const View = () => <div onClick={handleClick}>hi</div>;\n";
        let e = extract(Lang::Tsx, src).unwrap();
        assert!(names(&e.definitions).contains(&"View"));
    }

    #[test]
    fn python_definitions_and_calls() {
        let src = "\
def validate(token):
    return True

class Session:
    def start(self):
        validate(self.token)
";
        let e = extract(Lang::Python, src).unwrap();
        let n = names(&e.definitions);
        assert!(n.contains(&"validate"));
        assert!(n.contains(&"Session"));
        assert!(n.contains(&"start"));
        assert!(refs(&e).contains(&"validate"));
    }

    #[test]
    fn javascript_methods_and_news() {
        let src = "class A { go() { new Session(); helper(); } }\n";
        let e = extract(Lang::JavaScript, src).unwrap();
        assert!(names(&e.definitions).contains(&"A"));
        assert!(names(&e.definitions).contains(&"go"));
        assert!(refs(&e).contains(&"Session"));
        assert!(refs(&e).contains(&"helper"));
    }

    #[test]
    fn a_file_with_a_syntax_error_still_yields_what_parsed() {
        // Reviewing a branch mid-refactor is normal; refusing to index a file
        // that does not compile would blank the graph exactly when it is needed.
        let src = "fn good() {}\nfn broken( {\n";
        let e = extract(Lang::Rust, src).unwrap();
        assert!(names(&e.definitions).contains(&"good"));
    }

    #[test]
    fn extensions_map_to_the_right_grammar() {
        let lang = |p: &str| Lang::for_path(Path::new(p));
        assert_eq!(lang("src/a.rs"), Some(Lang::Rust));
        assert_eq!(lang("src/a.ts"), Some(Lang::TypeScript));
        assert_eq!(lang("src/a.tsx"), Some(Lang::Tsx));
        assert_eq!(lang("src/a.jsx"), Some(Lang::JavaScript));
        assert_eq!(lang("src/a.py"), Some(Lang::Python));
        assert_eq!(lang("README.md"), None);
        assert_eq!(lang("Makefile"), None);
    }

    /// A real 400-line file rather than a five-line fixture — grammar queries
    /// that look right on a snippet routinely miss things at scale.
    #[test]
    fn extracting_this_very_file_finds_its_own_symbols() {
        let e = extract(Lang::Rust, include_str!("extract.rs")).unwrap();
        let n = names(&e.definitions);
        for expected in [
            "extract",
            "def_patterns",
            "ref_patterns",
            "Lang",
            "Definition",
        ] {
            assert!(n.contains(&expected), "missing {expected}");
        }
        assert!(
            e.definitions.len() > 20,
            "expected many symbols, got {}",
            e.definitions.len()
        );
        assert!(
            refs(&e).contains(&"line_of"),
            "internal calls must be visible, or callers_of can never work"
        );
    }

    #[test]
    fn an_empty_file_is_not_an_error() {
        let e = extract(Lang::Rust, "").unwrap();
        assert!(e.definitions.is_empty());
        assert!(e.references.is_empty());
    }

    #[test]
    fn duplicate_matches_collapse_to_one_definition() {
        // `const f = () => {}` can match more than one pattern.
        let e = extract(Lang::TypeScript, "const f = () => {};\n").unwrap();
        assert_eq!(
            e.definitions.iter().filter(|d| d.name == "f").count(),
            1,
            "one symbol, not one per matching pattern"
        );
    }
}
