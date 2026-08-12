//! Prose review rules, authored as markdown and committed to the repo.
//!
//! `.diffmind/rules.toml` covers what a regex can decide: exact, free, and
//! deterministic. Plenty of a team's real standards are not like that — "public
//! handlers must return `ApiError`, never `anyhow::Error`" needs judgement. Those
//! belong here, in prose the model reads, versioned next to the code they govern.
//!
//! The split is deliberate and worth keeping: if a rule *can* be expressed as a
//! pattern, it belongs in `rules.toml` (or a linter), where it costs no tokens
//! and cannot be argued with.
//!
//! ## Why the body goes in the system prompt
//!
//! Rule sets carry `scope` globs, so which ones apply varies per file. The
//! obvious implementation — append the matching bodies to each unit's prompt —
//! makes the prompt prefix different for every unit and forfeits any chance of
//! prompt-prefix caching. Instead the analyzer groups units by which rule sets
//! apply to them, so every unit in a group sends a byte-identical prefix.

use crate::types::Severity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One `.md` file under `.diffmind/rules/`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rulebook {
    /// Stable name, used to attribute a finding and to suppress it. Defaults to
    /// the file stem.
    pub id: String,
    /// One line on what this rule set is for. Not sent to the model — it is
    /// documentation, shown by `diffmind rules list`.
    ///
    /// Cursor uses the equivalent field to let the agent *choose* rules at
    /// review time. That needs a multi-turn loop and a model good enough to
    /// pick well; against a 1.5B local model it would quietly skip the rules
    /// that mattered. Kept as documentation, and as the field that selection
    /// would use if a frontier backend ever makes it worthwhile.
    #[serde(default)]
    pub description: Option<String>,
    /// Globs this rule set applies to. Empty means the whole repository.
    ///
    /// A glob prefixed with `!` excludes instead: `["src/**", "!src/legacy/**"]`
    /// applies to `src` except the legacy tree. An exclusion always wins.
    #[serde(default)]
    pub scope: Vec<String>,
    /// Apply to every file regardless of `scope`.
    ///
    /// An empty `scope` already means "the whole repository", so this is not
    /// new capability — it is the difference between saying so and forgetting
    /// to. An always-on rule set is paid for on every single review, so it
    /// should be a decision, and `rules list` marks it as one.
    #[serde(default)]
    pub always: bool,
    /// Declared ceiling for findings attributed to this rule set. A rule set
    /// marked `low` cannot produce a `high` finding, which stops one opinionated
    /// style document from dominating a triage list.
    ///
    /// Also the drop order when the prompt budget cannot fit every rule set:
    /// higher severity is kept first.
    #[serde(default)]
    pub severity: Option<Severity>,
    /// The prose, verbatim, minus the front matter.
    pub body: String,
}

/// Prefix on the rule ID of a finding the model attributed to a rule set, so it
/// suppresses like any other: `// diffmind-ignore-next-line rulebook.api-conventions`.
pub const RULEBOOK_RULE_PREFIX: &str = "rulebook.";

impl Rulebook {
    pub fn rule_id(&self) -> String {
        format!("{RULEBOOK_RULE_PREFIX}{}", self.id)
    }

    /// Does this rule set govern `path`?
    ///
    /// A rule set with no `scope` — or one marked `always` — governs
    /// everything. An `!` exclusion beats every include, so `["src/**",
    /// "!src/legacy/**"]` reads the way a `.gitignore` would.
    pub fn applies_to(&self, path: &str) -> bool {
        let (excludes, includes): (Vec<&String>, Vec<&String>) =
            self.scope.iter().partition(|g| g.starts_with('!'));

        if excludes
            .iter()
            .any(|g| crate::detectors::file_matches_glob(path, g.trim_start_matches('!')))
        {
            return false;
        }

        if self.always || includes.is_empty() {
            return true;
        }

        includes
            .iter()
            .any(|g| crate::detectors::file_matches_glob(path, g))
    }

    /// Order for shedding rule sets when they do not all fit the prompt.
    ///
    /// Higher is kept longer. Severity leads because losing a `high` security
    /// rule set to an unranked style guide — which is what filename order did —
    /// is the one outcome nobody would choose.
    pub fn budget_rank(&self) -> u8 {
        match self.severity {
            Some(Severity::High) => 3,
            Some(Severity::Medium) => 2,
            Some(Severity::Low) => 1,
            None => 0,
        }
    }
}

/// Parse one markdown rule set. `default_id` is used when the front matter does
/// not name one — callers pass the file stem.
///
/// Returns `Err` with a human-readable reason rather than silently producing an
/// empty rule set: a rulebook that quietly does nothing is worse than one that
/// refuses to load.
pub fn parse(default_id: &str, text: &str) -> Result<Rulebook, String> {
    let (front_matter, body) = split_front_matter(text);

    let mut id = default_id.trim().to_string();
    let mut scope = Vec::new();
    let mut severity = None;
    let mut description = None;
    let mut always = false;

    for (key, value) in parse_front_matter(front_matter)? {
        match key.as_str() {
            "id" => {
                if !value.is_empty() {
                    id = value.join("");
                }
            }
            "description" => {
                let text = value.join(" ").trim().to_string();
                if !text.is_empty() {
                    description = Some(text);
                }
            }
            "always" => {
                let raw = value.join("").trim().to_lowercase();
                always = match raw.as_str() {
                    "true" | "yes" => true,
                    "false" | "no" | "" => false,
                    other => {
                        return Err(format!("`always` must be true or false, not `{other}`"));
                    }
                };
            }
            // Cursor's spelling, accepted so a rule set ported from
            // `.cursor/rules/` loads without an edit.
            "alwaysapply" | "alwaysApply" => {
                always = value.join("").trim().eq_ignore_ascii_case("true");
            }
            "scope" | "globs" => scope = value,
            "severity" => {
                let raw = value.join("");
                if !raw.trim().is_empty() {
                    severity = Some(Severity::parse(&raw));
                }
            }
            // `model:` is accepted and ignored: model tiering does not exist
            // yet, and rejecting the key would break every rulebook written
            // against the documented format once it does.
            "model" => {}
            other => {
                return Err(format!(
                    "unknown front-matter key `{other}`. Supported: id, description, \
                     scope (alias: globs), always (alias: alwaysApply), severity, model"
                ));
            }
        }
    }

    if id.is_empty() {
        return Err("rule set has no id and no filename to fall back on".into());
    }
    if body.trim().is_empty() {
        return Err("rule set has front matter but no rules under it".into());
    }
    // Every glob excluded and none included matches nothing at all, which is a
    // rule set that silently does no work — the failure this parser exists to
    // prevent.
    if !scope.is_empty() && !always && scope.iter().all(|g| g.starts_with('!')) {
        return Err(
            "`scope` has only `!` exclusions, so it matches no files. Add an include \
             glob, or set `always: true` to cover the repository minus the exclusions."
                .into(),
        );
    }

    Ok(Rulebook {
        id,
        description,
        scope,
        always,
        severity,
        body: body.trim().to_string(),
    })
}

/// Split `---\n…\n---\n` off the front. Returns `(front_matter, body)`; the
/// front matter is empty when the document has none.
fn split_front_matter(text: &str) -> (&str, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return ("", text);
    };

    // The closing fence is a line that is exactly `---`.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (&rest[..offset], &rest[offset + line.len()..]);
        }
        offset += line.len();
    }

    // Unterminated front matter: treat the whole document as body rather than
    // swallowing the rules into a header nobody reads.
    ("", text)
}

/// A deliberately small subset of YAML: `key: scalar`, `key: [a, b]`, and a
/// `key:` followed by indented `- item` lines. Anything else is an error the
/// user can act on, rather than a silently-dropped rule.
fn parse_front_matter(text: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        // Continuation of a block list.
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            let Some(last) = out.last_mut() else {
                return Err(format!("list item `{}` has no key above it", item.trim()));
            };
            last.1.push(unquote(item));
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("`{}` is not a `key: value` line", line.trim()));
        };
        let key = key.trim().to_lowercase();
        if key.is_empty() {
            return Err(format!("`{}` has an empty key", line.trim()));
        }
        out.push((key, parse_value(value.trim())));
    }

    Ok(out)
}

fn parse_value(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        return inner
            .split(',')
            .map(unquote)
            .filter(|s| !s.is_empty())
            .collect();
    }
    vec![unquote(value)]
}

fn unquote(value: &str) -> String {
    let v = value.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v);
    v.to_string()
}

/// The rule sets governing `path`, in the order they were loaded.
pub fn applicable<'a>(books: &'a [Rulebook], path: &str) -> Vec<&'a Rulebook> {
    books.iter().filter(|b| b.applies_to(path)).collect()
}

/// Identity of a set of rule sets, for the result cache.
///
/// Without this, editing a rulebook would serve cached findings produced by the
/// *previous* wording — the rules would appear to have no effect at all. Keyed
/// on the applicable set only, so tightening a rule scoped to `src/api/**`
/// does not invalidate anything outside it.
pub fn digest(books: &[&Rulebook]) -> String {
    if books.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    for b in books {
        h.update(b.id.as_bytes());
        h.update(b"\x00");
        h.update(b.body.as_bytes());
        h.update(b"\x00");
        if let Some(s) = b.severity {
            h.update(s.as_str().as_bytes());
        }
        h.update(b"\x00");
    }
    format!("{:x}", h.finalize())[..16].to_string()
}

/// Stable key identifying *which* rule sets apply, used to group units so that
/// every unit in a group renders a byte-identical prompt prefix.
pub fn group_key(books: &[&Rulebook]) -> String {
    books
        .iter()
        .map(|b| b.id.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The starter file `diffmind rules init` writes. Mirrors the review focus that
/// is already built into the prompt, so an untouched copy changes nothing and a
/// user can see exactly what they are overriding.
pub const DEFAULT_RULEBOOK: &str = r#"---
# One line on what this covers. Documentation only — `diffmind rules list`
# shows it. Not sent to the model.
# description: House rules for the API layer
#
# Globs this rule set applies to. Omit to cover the whole repository.
# Prefix a glob with `!` to exclude: ["src/**", "!src/legacy/**"].
# scope: ["src/**/*.rs"]
#
# Apply to every file, whatever `scope` says. An always-on rule set is paid
# for on every review, so make it a decision.
# always: false
#
# Ceiling for findings attributed to this rule set: high, medium, or low.
# Also the drop order when rule sets do not all fit the prompt — highest kept.
severity: medium
---

# Review standards

- Errors must be handled or deliberately propagated, never silently swallowed.
- Public functions that can fail should return a typed error, not a string.
- New behaviour needs a test; changed behaviour needs its test updated.
- Anything reading user input must validate it before use.
- Prefer clarity over cleverness: a reviewer should not need the author present.

Delete what does not apply to this repository and add what does. This file is
prose — the model reads it. Rules that a regex could decide belong in
`.diffmind/rules.toml`, where they cost nothing to check.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"---
id: api-conventions
scope: ["src/api/**/*.rs", "src/handlers/*.rs"]
severity: high
---

# API conventions

- Public handlers must return `ApiError`.
"#;

    #[test]
    fn parses_front_matter_and_body() {
        let rb = parse("ignored", FULL).unwrap();
        assert_eq!(rb.id, "api-conventions");
        assert_eq!(rb.scope, ["src/api/**/*.rs", "src/handlers/*.rs"]);
        assert_eq!(rb.severity, Some(Severity::High));
        assert!(rb.body.starts_with("# API conventions"));
        assert!(
            !rb.body.contains("scope:"),
            "front matter must not leak into the prompt"
        );
    }

    #[test]
    fn the_filename_supplies_the_id_when_front_matter_does_not() {
        let rb = parse("security", "---\nseverity: high\n---\n\n- No secrets.\n").unwrap();
        assert_eq!(rb.id, "security");
        assert_eq!(rb.rule_id(), "rulebook.security");
    }

    #[test]
    fn a_document_with_no_front_matter_is_all_body() {
        let rb = parse("house-style", "# House style\n\n- Be kind.\n").unwrap();
        assert_eq!(rb.id, "house-style");
        assert!(rb.scope.is_empty(), "no scope means the whole repo");
        assert_eq!(rb.severity, None);
        assert!(rb.body.contains("Be kind"));
    }

    #[test]
    fn block_style_lists_parse_too() {
        let rb = parse(
            "x",
            "---\nscope:\n  - src/a/**\n  - src/b/**\n---\n\n- Rule.\n",
        )
        .unwrap();
        assert_eq!(rb.scope, ["src/a/**", "src/b/**"]);
    }

    #[test]
    fn an_empty_rule_set_is_rejected_rather_than_loaded_silently() {
        // A file with rules only in the front matter does nothing at all. Better
        // to say so than to let the user believe it is in force.
        let err = parse("x", "---\nseverity: high\n---\n\n   \n").unwrap_err();
        assert!(err.contains("no rules"), "got: {err}");
    }

    #[test]
    fn an_unknown_front_matter_key_is_reported() {
        let err = parse("x", "---\nsevrity: high\n---\n\n- Rule.\n").unwrap_err();
        assert!(err.contains("sevrity"), "got: {err}");
        assert!(
            err.contains("Supported"),
            "the error should list valid keys"
        );
    }

    #[test]
    fn the_model_key_is_accepted_and_ignored() {
        // Documented in the format from the start; tiering lands later. Rejecting
        // it would break rulebooks written against the docs.
        let rb = parse("x", "---\nmodel: default\n---\n\n- Rule.\n").unwrap();
        assert_eq!(rb.id, "x");
    }

    #[test]
    fn unterminated_front_matter_does_not_swallow_the_rules() {
        let rb = parse("x", "---\nscope: [a]\n\n# Rules\n\n- Do the thing.\n").unwrap();
        assert!(
            rb.body.contains("Do the thing"),
            "the rules must survive a missing closing fence"
        );
    }

    fn book(id: &str, scope: &[&str]) -> Rulebook {
        Rulebook {
            id: id.into(),
            description: None,
            always: false,
            scope: scope.iter().map(|s| s.to_string()).collect(),
            severity: None,
            body: format!("body of {id}"),
        }
    }

    /// CodeRabbit's `path_filters` spelling: an `!` glob subtracts. Without it
    /// the only way to exempt a legacy tree was to enumerate everything else.
    #[test]
    fn a_negated_glob_excludes_and_beats_every_include() {
        let b = book("api", &["src/**", "!src/legacy/**"]);

        assert!(b.applies_to("src/api/handler.ts"));
        assert!(
            !b.applies_to("src/legacy/old.ts"),
            "an exclusion must win over the include that also matches"
        );
        assert!(!b.applies_to("docs/readme.ts"), "outside every include");
    }

    /// `always` plus exclusions is the "whole repo except" shape.
    #[test]
    fn always_covers_everything_but_still_honours_exclusions() {
        let b = Rulebook {
            always: true,
            ..book("house", &["!vendor/**"])
        };

        assert!(b.applies_to("anything/at/all.rs"));
        assert!(!b.applies_to("vendor/dep.rs"));
    }

    /// A scope of nothing but exclusions matches no file at all — a rule set
    /// that silently does no work, which this parser exists to refuse.
    #[test]
    fn a_scope_of_only_exclusions_is_rejected() {
        let err =
            parse("x", "---\nscope: [\"!src/**\"]\n---\n\n- A rule.\n").expect_err("must not load");
        assert!(err.contains("matches no files"), "{err}");
    }

    #[test]
    fn cursor_spellings_are_accepted_so_a_ported_rule_set_loads() {
        let b = parse(
            "ported",
            "---\ndescription: Ported from Cursor\nglobs: [\"**/*.ts\"]\nalwaysApply: false\n---\n\n- A rule.\n",
        )
        .expect("cursor front matter should load");

        assert_eq!(b.description.as_deref(), Some("Ported from Cursor"));
        assert_eq!(b.scope, ["**/*.ts"]);
        assert!(!b.always);
    }

    #[test]
    fn budget_rank_orders_by_declared_severity() {
        assert!(
            book("a", &[]).budget_rank()
                < Rulebook {
                    severity: Some(Severity::Low),
                    ..book("b", &[])
                }
                .budget_rank(),
            "an unranked rule set is shed before one the author graded"
        );
    }

    #[test]
    fn scope_selects_which_rule_sets_govern_a_file() {
        let books = vec![
            book("global", &[]),
            book("api", &["src/api/**"]),
            book("web", &["src/web/**"]),
        ];

        let ids: Vec<&str> = applicable(&books, "src/api/users.rs")
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["global", "api"]);

        let ids: Vec<&str> = applicable(&books, "README.md")
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["global"], "an unscoped rule set covers everything");
    }

    #[test]
    fn the_digest_moves_when_the_prose_changes() {
        let a = book("api", &[]);
        let mut b = a.clone();
        b.body = "different rules".into();

        assert_ne!(
            digest(&[&a]),
            digest(&[&b]),
            "editing a rulebook must invalidate its cached findings, or the \
             edit appears to do nothing"
        );
        assert_eq!(digest(&[&a]), digest(&[&a.clone()]));
        assert!(
            digest(&[]).is_empty(),
            "no rules, no cache-key contribution"
        );
    }

    #[test]
    fn the_digest_ignores_rule_sets_that_do_not_apply() {
        let api = book("api", &["src/api/**"]);
        let web = book("web", &["src/web/**"]);
        let books = vec![api.clone(), web.clone()];

        let before = digest(&applicable(&books, "src/api/users.rs"));

        let mut edited_web = web.clone();
        edited_web.body = "rewritten".into();
        let books = vec![api, edited_web];
        let after = digest(&applicable(&books, "src/api/users.rs"));

        assert_eq!(
            before, after,
            "rewriting a rule set scoped elsewhere must not re-review this file"
        );
    }

    #[test]
    fn group_key_distinguishes_rule_set_combinations() {
        let global = book("global", &[]);
        let api = book("api", &["src/api/**"]);
        assert_eq!(group_key(&[&global, &api]), "global,api");
        assert_ne!(group_key(&[&global]), group_key(&[&global, &api]));
    }

    #[test]
    fn the_scaffolded_default_parses() {
        let rb = parse("default", DEFAULT_RULEBOOK).expect("shipped default must load");
        assert_eq!(rb.severity, Some(Severity::Medium));
        assert!(rb.scope.is_empty(), "the starter file covers everything");
        assert!(rb.body.contains("Review standards"));
    }
}
