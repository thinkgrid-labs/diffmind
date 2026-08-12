//! Tree-sitter symbol extraction: source text in, definitions and references out.
//!
//! This replaces a pile of regexes that could only see `pub`/`export` symbols
//! and had no concept of a reference at all. The regex index could answer "where
//! is `validateToken` defined"; it could never answer "what calls it", which is
//! the question a reviewer actually has about a changed function.
//!
//! # Adding a language
//!
//! One entry in [`LANGUAGES`], and nothing else. Add the grammar crate, then:
//!
//! ```ignore
//! LangSpec {
//!     name: "kotlin",
//!     extensions: &["kt", "kts"],
//!     grammar: || tree_sitter_kotlin::LANGUAGE.into(),
//!     defs: &[def("function", "(function_declaration ...) @def")],
//!     refs: &["(call_expression ...) @ref"],
//! }
//! ```
//!
//! A definition pattern captures the whole declaration as `@def` and its name as
//! `@name`; the range comes from `@def` so "which symbol encloses line N" has a
//! real body to test against. Reference patterns capture one `@ref` each and
//! should stay narrow — call targets, constructors, type positions. Capturing
//! every bare identifier makes `callers_of` return most of the repository, and a
//! blast radius that includes everything is the same as none.
//!
//! # Tolerant at runtime, strict in tests
//!
//! Each pattern is compiled on its own and one that fails is skipped rather than
//! taking the whole language down with it. Grammar crates rename nodes between
//! versions, and losing one kind of definition is far better than losing every
//! symbol in the repository the day a dependency is bumped.
//! `every_pattern_compiles` then fails loudly in CI, so the degradation is never
//! silent — and a contributor's new pattern is checked against the real grammar.

// Consumed by the graph store.
#![allow(dead_code)]

use std::path::Path;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// A definition pattern: `@def` is the declaration, `@name` is its name.
pub struct DefPattern {
    kind: &'static str,
    query: &'static str,
}

const fn def(kind: &'static str, query: &'static str) -> DefPattern {
    DefPattern { kind, query }
}

/// Everything needed to extract symbols from one language.
pub struct LangSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    grammar: fn() -> tree_sitter::Language,
    defs: &'static [DefPattern],
    refs: &'static [&'static str],
}

/// A language this build can parse. Cheap to copy — it is a pointer to a
/// [`LangSpec`] in [`LANGUAGES`].
#[derive(Clone, Copy)]
pub struct Lang(&'static LangSpec);

impl std::fmt::Debug for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.name)
    }
}

impl Lang {
    pub fn for_path(path: &Path) -> Option<Lang> {
        Self::for_extension(path.extension()?.to_str()?)
    }

    pub fn for_extension(ext: &str) -> Option<Lang> {
        LANGUAGES
            .iter()
            .find(|spec| spec.extensions.contains(&ext))
            .map(Lang)
    }

    pub fn name(self) -> &'static str {
        self.0.name
    }

    fn grammar(self) -> tree_sitter::Language {
        (self.0.grammar)()
    }

    /// Every language this build can parse.
    pub fn all() -> impl Iterator<Item = Lang> {
        LANGUAGES.iter().map(Lang)
    }

    /// Names of every supported language, for `--help` and the README.
    pub fn names() -> Vec<&'static str> {
        LANGUAGES.iter().map(|s| s.name).collect()
    }
}

impl PartialEq for Lang {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}
impl Eq for Lang {}

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

// ─── The table ───────────────────────────────────────────────────────────────

pub static LANGUAGES: &[LangSpec] = &[
    LangSpec {
        name: "rust",
        extensions: &["rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        defs: &[
            def("function", "(function_item name: (identifier) @name) @def"),
            def("struct", "(struct_item name: (type_identifier) @name) @def"),
            def("enum", "(enum_item name: (type_identifier) @name) @def"),
            def("trait", "(trait_item name: (type_identifier) @name) @def"),
            def("type", "(type_item name: (type_identifier) @name) @def"),
            def("const", "(const_item name: (identifier) @name) @def"),
            def("static", "(static_item name: (identifier) @name) @def"),
            def("macro", "(macro_definition name: (identifier) @name) @def"),
            def("module", "(mod_item name: (identifier) @name) @def"),
        ],
        refs: &[
            "(call_expression function: (identifier) @ref)",
            "(call_expression function: (field_expression field: (field_identifier) @ref))",
            "(call_expression function: (scoped_identifier name: (identifier) @ref))",
            "(macro_invocation macro: (identifier) @ref)",
            "(type_identifier) @ref",
        ],
    },
    LangSpec {
        name: "typescript",
        extensions: &["ts", "mts", "cts"],
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        defs: TS_DEFS,
        refs: TS_REFS,
    },
    LangSpec {
        name: "tsx",
        extensions: &["tsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        defs: TS_DEFS,
        refs: TS_REFS,
    },
    LangSpec {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        defs: &[
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
        ],
        // No `type_identifier`: that node does not exist in the JavaScript
        // grammar, and including it would fail to compile.
        refs: &[
            "(call_expression function: (identifier) @ref)",
            "(call_expression function: (member_expression property: (property_identifier) @ref))",
            "(new_expression constructor: (identifier) @ref)",
        ],
    },
    LangSpec {
        name: "python",
        extensions: &["py", "pyi"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        defs: &[
            def(
                "function",
                "(function_definition name: (identifier) @name) @def",
            ),
            def("class", "(class_definition name: (identifier) @name) @def"),
        ],
        refs: &[
            "(call function: (identifier) @ref)",
            "(call function: (attribute attribute: (identifier) @ref))",
        ],
    },
    LangSpec {
        name: "go",
        extensions: &["go"],
        grammar: || tree_sitter_go::LANGUAGE.into(),
        defs: &[
            def(
                "function",
                "(function_declaration name: (identifier) @name) @def",
            ),
            def(
                "method",
                "(method_declaration name: (field_identifier) @name) @def",
            ),
            def("type", "(type_spec name: (type_identifier) @name) @def"),
        ],
        refs: &[
            "(call_expression function: (identifier) @ref)",
            "(call_expression function: (selector_expression field: (field_identifier) @ref))",
            "(type_identifier) @ref",
        ],
    },
    LangSpec {
        name: "java",
        extensions: &["java"],
        grammar: || tree_sitter_java::LANGUAGE.into(),
        defs: &[
            def("class", "(class_declaration name: (identifier) @name) @def"),
            def(
                "interface",
                "(interface_declaration name: (identifier) @name) @def",
            ),
            def("enum", "(enum_declaration name: (identifier) @name) @def"),
            def(
                "record",
                "(record_declaration name: (identifier) @name) @def",
            ),
            def(
                "method",
                "(method_declaration name: (identifier) @name) @def",
            ),
            def(
                "constructor",
                "(constructor_declaration name: (identifier) @name) @def",
            ),
        ],
        refs: &[
            "(method_invocation name: (identifier) @ref)",
            "(object_creation_expression type: (type_identifier) @ref)",
            "(type_identifier) @ref",
        ],
    },
    LangSpec {
        name: "c#",
        extensions: &["cs"],
        grammar: || tree_sitter_c_sharp::LANGUAGE.into(),
        defs: &[
            def("class", "(class_declaration name: (identifier) @name) @def"),
            def(
                "interface",
                "(interface_declaration name: (identifier) @name) @def",
            ),
            def(
                "struct",
                "(struct_declaration name: (identifier) @name) @def",
            ),
            def("enum", "(enum_declaration name: (identifier) @name) @def"),
            def(
                "record",
                "(record_declaration name: (identifier) @name) @def",
            ),
            def(
                "method",
                "(method_declaration name: (identifier) @name) @def",
            ),
            def(
                "constructor",
                "(constructor_declaration name: (identifier) @name) @def",
            ),
            def(
                "property",
                "(property_declaration name: (identifier) @name) @def",
            ),
        ],
        refs: &[
            "(invocation_expression function: (identifier) @ref)",
            "(invocation_expression function: (member_access_expression name: (identifier) @ref))",
        ],
    },
    LangSpec {
        name: "ruby",
        extensions: &["rb", "rake"],
        grammar: || tree_sitter_ruby::LANGUAGE.into(),
        defs: &[
            def("method", "(method name: (identifier) @name) @def"),
            def("method", "(singleton_method name: (identifier) @name) @def"),
            def("class", "(class name: (constant) @name) @def"),
            def("module", "(module name: (constant) @name) @def"),
        ],
        // A paren-less Ruby call (`check`) is grammatically identical to a local
        // variable reference, so only explicit calls — `check(...)` or
        // `obj.check` — become edges. Capturing bare identifiers instead would
        // make every local variable look like a call.
        refs: &["(call method: (identifier) @ref)"],
    },
    LangSpec {
        name: "php",
        extensions: &["php"],
        grammar: || tree_sitter_php::LANGUAGE_PHP.into(),
        defs: &[
            def("function", "(function_definition name: (name) @name) @def"),
            def("method", "(method_declaration name: (name) @name) @def"),
            def("class", "(class_declaration name: (name) @name) @def"),
            def(
                "interface",
                "(interface_declaration name: (name) @name) @def",
            ),
            def("trait", "(trait_declaration name: (name) @name) @def"),
        ],
        refs: &[
            "(function_call_expression function: (name) @ref)",
            "(member_call_expression name: (name) @ref)",
        ],
    },
    LangSpec {
        name: "c",
        extensions: &["c", "h"],
        grammar: || tree_sitter_c::LANGUAGE.into(),
        defs: &[
            def(
                "function",
                "(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def",
            ),
            def(
                "struct",
                "(struct_specifier name: (type_identifier) @name) @def",
            ),
            def(
                "enum",
                "(enum_specifier name: (type_identifier) @name) @def",
            ),
            def(
                "type",
                "(type_definition declarator: (type_identifier) @name) @def",
            ),
        ],
        refs: &[
            "(call_expression function: (identifier) @ref)",
            "(type_identifier) @ref",
        ],
    },
    LangSpec {
        name: "c++",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh"],
        grammar: || tree_sitter_cpp::LANGUAGE.into(),
        defs: &[
            def(
                "function",
                "(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def",
            ),
            def(
                "class",
                "(class_specifier name: (type_identifier) @name) @def",
            ),
            def(
                "struct",
                "(struct_specifier name: (type_identifier) @name) @def",
            ),
            def(
                "enum",
                "(enum_specifier name: (type_identifier) @name) @def",
            ),
        ],
        refs: &[
            "(call_expression function: (identifier) @ref)",
            "(call_expression function: (field_expression field: (field_identifier) @ref))",
            "(type_identifier) @ref",
        ],
    },
    LangSpec {
        name: "scala",
        extensions: &["scala", "sc"],
        grammar: || tree_sitter_scala::LANGUAGE.into(),
        defs: &[
            def(
                "function",
                "(function_definition name: (identifier) @name) @def",
            ),
            def("class", "(class_definition name: (identifier) @name) @def"),
            def(
                "object",
                "(object_definition name: (identifier) @name) @def",
            ),
            def("trait", "(trait_definition name: (identifier) @name) @def"),
        ],
        refs: &["(call_expression function: (identifier) @ref)"],
    },
];

/// TypeScript and TSX differ only in grammar, never in what a symbol looks like.
static TS_DEFS: &[DefPattern] = &[
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

static TS_REFS: &[&str] = &[
    "(call_expression function: (identifier) @ref)",
    "(call_expression function: (member_expression property: (property_identifier) @ref))",
    "(new_expression constructor: (identifier) @ref)",
    "(type_identifier) @ref",
];

fn def_patterns(lang: Lang) -> &'static [DefPattern] {
    lang.0.defs
}

fn ref_patterns(lang: Lang) -> &'static [&'static str] {
    lang.0.refs
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

    /// Look a language up the way production does — by extension, from the
    /// table. Keeps the tests honest about the one source of truth.
    fn lang(ext: &str) -> Lang {
        Lang::for_extension(ext).unwrap_or_else(|| panic!("no language for .{ext}"))
    }

    fn names(defs: &[Definition]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    fn refs(e: &Extracted) -> Vec<&str> {
        e.references.iter().map(|r| r.name.as_str()).collect()
    }

    /// A grammar bump that renames a node would otherwise silently stop
    /// extracting one kind of symbol. Runtime skips the broken pattern; this
    /// makes CI say so.
    /// A grammar bump that renames a node would otherwise silently stop
    /// extracting one kind of symbol. Runtime skips the broken pattern; this
    /// makes CI say so — and checks a contributor's new language for real.
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

    /// Every language in the table must actually declare an extension, or it is
    /// unreachable — `for_path` is the only way in.
    #[test]
    fn every_language_is_reachable_by_extension() {
        for spec in LANGUAGES {
            assert!(
                !spec.extensions.is_empty(),
                "{} has no extensions and can never be selected",
                spec.name
            );
            for ext in spec.extensions {
                assert_eq!(
                    Lang::for_extension(ext).map(|l| l.name()),
                    Some(spec.name),
                    ".{ext} should map to {}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn no_two_languages_claim_the_same_extension() {
        let mut seen = std::collections::HashMap::new();
        for spec in LANGUAGES {
            for ext in spec.extensions {
                if let Some(other) = seen.insert(*ext, spec.name) {
                    panic!("both {other} and {} claim .{ext}", spec.name);
                }
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
        let e = extract(lang("rs"), src).unwrap();
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
        let e = extract(lang("rs"), "fn helper() {}\n").unwrap();
        assert_eq!(names(&e.definitions), ["helper"]);
    }

    #[test]
    fn a_definition_spans_its_whole_body() {
        let src = "fn outer() {\n    let a = 1;\n    let b = 2;\n}\n";
        let d = &extract(lang("rs"), src).unwrap().definitions[0];
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
        let e = extract(lang("ts"), src).unwrap();
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
        let e = extract(lang("tsx"), src).unwrap();
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
        let e = extract(lang("py"), src).unwrap();
        let n = names(&e.definitions);
        assert!(n.contains(&"validate"));
        assert!(n.contains(&"Session"));
        assert!(n.contains(&"start"));
        assert!(refs(&e).contains(&"validate"));
    }

    #[test]
    fn javascript_methods_and_news() {
        let src = "class A { go() { new Session(); helper(); } }\n";
        let e = extract(lang("js"), src).unwrap();
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
        let e = extract(lang("rs"), src).unwrap();
        assert!(names(&e.definitions).contains(&"good"));
    }

    /// A pattern can compile against a grammar and still match nothing. Every
    /// language must actually produce a definition *and* a call edge, or the
    /// blast radius silently does not work for it.
    #[test]
    fn every_language_extracts_a_definition_and_a_call() {
        struct Case {
            ext: &'static str,
            source: &'static str,
            wants_def: &'static str,
            wants_ref: &'static str,
        }
        let cases = [
            Case {
                ext: "rs",
                wants_def: "login",
                wants_ref: "check",
                source: "fn check() -> bool { true }\npub fn login() { check(); }\n",
            },
            Case {
                ext: "ts",
                wants_def: "login",
                wants_ref: "check",
                source: "function check(): boolean { return true; }\nexport function login() { check(); }\n",
            },
            Case {
                ext: "tsx",
                wants_def: "View",
                wants_ref: "check",
                source: "export const View = () => { check(); return <div/>; };\n",
            },
            Case {
                ext: "js",
                wants_def: "login",
                wants_ref: "check",
                source: "function check() { return true; }\nfunction login() { check(); }\n",
            },
            Case {
                ext: "py",
                wants_def: "login",
                wants_ref: "check",
                source: "def check():\n    return True\n\ndef login():\n    check()\n",
            },
            Case {
                ext: "go",
                wants_def: "Login",
                wants_ref: "Check",
                source: "package main\nfunc Check() bool { return true }\nfunc Login() { Check() }\n",
            },
            Case {
                ext: "java",
                wants_def: "login",
                wants_ref: "check",
                source: "class A { boolean check() { return true; } void login() { check(); } }\n",
            },
            Case {
                ext: "cs",
                wants_def: "Login",
                wants_ref: "Check",
                source: "class A { bool Check() { return true; } void Login() { Check(); } }\n",
            },
            Case {
                ext: "rb",
                wants_def: "login",
                wants_ref: "check",
                // An explicit receiver. A paren-less `check` is grammatically a
                // local variable reference in Ruby — see the note on its refs.
                source: "def login\n  Validator.check(token)\nend\n",
            },
            Case {
                ext: "php",
                wants_def: "login",
                wants_ref: "check",
                source: "<?php\nfunction check() { return true; }\nfunction login() { check(); }\n",
            },
            Case {
                ext: "c",
                wants_def: "login",
                wants_ref: "check",
                source: "int check(void) { return 1; }\nvoid login(void) { check(); }\n",
            },
            Case {
                ext: "cpp",
                wants_def: "login",
                wants_ref: "check",
                source: "bool check() { return true; }\nvoid login() { check(); }\n",
            },
            Case {
                ext: "scala",
                wants_def: "login",
                wants_ref: "check",
                source: "object A {\n  def check(): Boolean = true\n  def login(): Unit = check()\n}\n",
            },
        ];

        assert_eq!(cases.len(), LANGUAGES.len(), "every language needs a case");

        for c in &cases {
            let l = lang(c.ext);
            let e = extract(l, c.source).unwrap_or_else(|| panic!("{} failed to parse", l.name()));
            assert!(
                names(&e.definitions).contains(&c.wants_def),
                "{}: no definition `{}` in {:?}",
                l.name(),
                c.wants_def,
                names(&e.definitions)
            );
            assert!(
                refs(&e).contains(&c.wants_ref),
                "{}: no call edge to `{}` in {:?} — blast radius will not work",
                l.name(),
                c.wants_ref,
                refs(&e)
            );
        }
    }

    #[test]
    fn extensions_map_to_the_right_grammar() {
        let of = |p: &str| Lang::for_path(Path::new(p)).map(|l| l.name());
        assert_eq!(of("src/a.rs"), Some("rust"));
        assert_eq!(of("src/a.ts"), Some("typescript"));
        assert_eq!(of("src/a.tsx"), Some("tsx"));
        assert_eq!(of("src/a.jsx"), Some("javascript"));
        assert_eq!(of("src/a.py"), Some("python"));
        assert_eq!(of("main.go"), Some("go"));
        assert_eq!(of("App.java"), Some("java"));
        assert_eq!(of("Program.cs"), Some("c#"));
        assert_eq!(of("app.rb"), Some("ruby"));
        assert_eq!(of("index.php"), Some("php"));
        assert_eq!(of("main.c"), Some("c"));
        assert_eq!(of("main.cpp"), Some("c++"));
        assert_eq!(of("Main.scala"), Some("scala"));
        assert_eq!(of("README.md"), None);
        assert_eq!(of("Makefile"), None);
    }

    /// A real 400-line file rather than a five-line fixture — grammar queries
    /// that look right on a snippet routinely miss things at scale.
    #[test]
    fn extracting_this_very_file_finds_its_own_symbols() {
        let e = extract(lang("rs"), include_str!("extract.rs")).unwrap();
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
        let e = extract(lang("rs"), "").unwrap();
        assert!(e.definitions.is_empty());
        assert!(e.references.is_empty());
    }

    #[test]
    fn duplicate_matches_collapse_to_one_definition() {
        // `const f = () => {}` can match more than one pattern.
        let e = extract(lang("ts"), "const f = () => {};\n").unwrap();
        assert_eq!(
            e.definitions.iter().filter(|d| d.name == "f").count(),
            1,
            "one symbol, not one per matching pattern"
        );
    }
}
