//! Prompt construction, kept separate from inference so the exact text a
//! backend receives can be asserted in tests without loading a 1.1 GB model.

use crate::rulebook::Rulebook;

/// Render the rule sets governing this unit into the stable half of the prompt.
///
/// Each is labelled with its id so the model can attribute a finding back to it,
/// and with its declared severity so the ceiling is stated rather than merely
/// enforced after the fact.
fn render_rulebooks(books: &[&Rulebook], max_bytes: usize) -> String {
    if books.is_empty() {
        return String::new();
    }

    let mut out = String::from("\n\n### Project review rules\n");
    for book in books {
        let label = match book.severity {
            Some(s) => format!("{} ({} severity)", book.id, s.as_str()),
            None => book.id.clone(),
        };
        let entry = format!("\n--- {label} ---\n{}\n", book.body);
        // Stop at a whole rule set rather than truncating one mid-sentence: half
        // a rule reads as a complete rule that says something else.
        if out.len() + entry.len() > max_bytes {
            break;
        }
        out.push_str(&entry);
    }

    if out.trim() == "### Project review rules" {
        return String::new();
    }
    out
}

/// Bumped whenever a prompt's wording changes. It is part of the review cache
/// key, so a prompt edit invalidates stale cached results instead of silently
/// serving output the current prompt would never produce.
pub const PROMPT_VERSION: u32 = 4;

/// A chat-shaped prompt. Backends render it their own way: the local GGUF
/// model needs ChatML control tokens, an HTTP endpoint needs a messages array.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub system: String,
    pub user: String,
}

impl Prompt {
    /// Render in Qwen's ChatML format for the local model.
    pub fn to_chatml(&self) -> String {
        format!(
            "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            self.system, self.user
        )
    }

    /// Rough token estimate for budgeting when no tokenizer is available.
    /// Deliberately pessimistic — code tokenizes worse than prose.
    pub fn estimated_tokens(&self) -> usize {
        (self.system.len() + self.user.len()) / 3
    }
}

/// Truncates `s` to at most `max_bytes` bytes, stepping back to the nearest
/// valid UTF-8 character boundary so the result is always valid UTF-8.
pub fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Inputs that shape a review prompt.
///
/// The split between `system` and `user` in the result is not cosmetic. Every
/// field that is constant across a run belongs in the system half so that half
/// is byte-identical from one unit to the next and can be prefix-cached; the
/// per-unit data (this unit's diff, and the context assembled for it) goes in
/// the user half. The context section used to sit in the system prompt, which
/// silently made every unit's prefix unique.
pub struct ReviewPromptInput<'a> {
    pub diff: &'a str,
    /// Symbol definitions and enclosing-function bodies pulled from the index.
    pub context: &'a str,
    /// Detected languages, e.g. "Rust, TypeScript".
    pub languages: Option<&'a str>,
    /// User story / acceptance criteria from a ticket.
    pub requirements: Option<&'a str>,
    /// Project rule sets that govern this unit's file.
    pub rulebooks: &'a [&'a Rulebook],
    /// Byte budget for the context section.
    pub max_context_bytes: usize,
    /// Byte budget for the requirements section.
    pub max_requirements_bytes: usize,
    /// Byte budget for the rendered rule sets.
    pub max_rules_bytes: usize,
}

pub fn review_prompt(input: &ReviewPromptInput<'_>) -> Prompt {
    let stack = input
        .languages
        .unwrap_or("TypeScript, JavaScript, Rust, Go, Python");

    let requirements_section = match input.requirements {
        Some(req) if !req.trim().is_empty() => format!(
            "\n\n### User Story / Acceptance Criteria:\n{}\n",
            truncate_to_char_boundary(req, input.max_requirements_bytes)
        ),
        _ => String::new(),
    };

    let context_section = {
        let ctx = truncate_to_char_boundary(input.context, input.max_context_bytes);
        if ctx.trim().is_empty() {
            String::new()
        } else {
            format!("### Surrounding code (for reference, not under review):\n{ctx}\n\n")
        }
    };

    let rules_section = render_rulebooks(input.rulebooks, input.max_rules_bytes);
    let has_rules = !rules_section.is_empty();
    let has_requirements = !requirements_section.is_empty();

    let compliance_focus = if has_requirements {
        "\n5. **Compliance**: Does the diff satisfy every acceptance criterion? \
         Flag missing or incomplete requirements as category \"compliance\"."
    } else {
        ""
    };

    let category_hint = if has_requirements {
        "\"security\"|\"quality\"|\"performance\"|\"maintainability\"|\"compliance\""
    } else {
        "\"security\"|\"quality\"|\"performance\"|\"maintainability\""
    };

    let rule_field = if has_rules {
        ", \"rule\": \"id of the project rule set this violates, omitted otherwise\""
    } else {
        ""
    };
    let rule_guidance = if has_rules {
        "\n- rule: set it only when the finding breaks one of the project rules above, \
         copying that rule set's name exactly. Omit it for everything else.\n\
         - Project rules are this team's standards. Apply them in addition to the focus \
         areas above, not instead of them."
    } else {
        ""
    };

    let system = format!(
        "You are an expert Senior Software Engineer and Code Reviewer. Analyze the git diff and \
         provide a thorough code review for {stack} code.{requirements_section}{rules_section}\n\n\
         Focus on:\n\
         1. **Security**: Vulnerabilities, exposed secrets, insecure handling, disabled auth or validation.\n\
         2. **Quality**: Bugs, anti-patterns, logical errors, dead code left behind.\n\
         3. **Performance**: Bottlenecks, inefficient algorithms or queries.\n\
         4. **Maintainability**: Hard-to-read code, poor naming, high complexity.{compliance_focus}\n\n\
         Return a JSON object ONLY with exactly this structure:\n\
         {{\n\
         \x20 \"findings\": [{{ \"file\": \"path\", \"line\": 12, \"severity\": \"high\"|\"medium\"|\"low\", \
         \"category\": {category_hint}, \"issue\": \"description\", \"suggested_fix\": \"fix\"{rule_field} }}],\n\
         \x20 \"positives\": [\"one sentence describing something done well\"],\n\
         \x20 \"suggestions\": [\"one sentence optional improvement that is not a bug\"]\n\
         }}\n\n\
         Rules:\n\
         - findings: real issues only. Use [] if there are none. Do not invent problems.\n\
         - file: copy the path exactly as it appears in the diff header.\n\
         - line: use the line number shown in the diff for the line you are describing.\n\
         - positives: always include 1-3 things done well. Never leave this empty.\n\
         - suggestions: nice-to-have improvements (tests, docs, refactors). Use [] if nothing comes to mind.\n\
         - Keep each positive/suggestion to one concise sentence.{rule_guidance}\n\
         - Output the JSON object and nothing else — no preamble, no commentary, no markdown fence."
    );

    Prompt {
        system,
        // Everything below varies per unit, which is exactly why it is here and
        // not in the system half.
        user: format!("{context_section}Analyze this diff:\n{}\n", input.diff),
    }
}

/// A cheap pass that decides which files in a large diff deserve deep review.
/// Rating every hunk with the full prompt is slow and pushes the model to
/// manufacture findings about whitespace changes.
pub fn triage_prompt(file_summaries: &str) -> Prompt {
    Prompt {
        system: "You are a senior engineer triaging a pull request. Given a list of changed files \
                 with their change sizes and a short preview, decide which files carry real risk \
                 (logic, security, data handling, concurrency) and which are low-risk \
                 (formatting, comments, lockfiles, generated code, simple renames).\n\n\
                 Return a JSON object ONLY:\n\
                 {\"review\": [\"path/one.rs\", \"path/two.ts\"], \"skip\": [\"path/three.md\"]}\n\n\
                 Rules:\n\
                 - Every path from the input must appear in exactly one list.\n\
                 - Copy paths verbatim.\n\
                 - When uncertain, put the file in \"review\".\n\
                 - Output the JSON object and nothing else."
            .to_string(),
        user: format!("Changed files:\n{file_summaries}\n"),
    }
}

pub fn pr_description_prompt(diff: &str, ticket: Option<&str>) -> Prompt {
    let ticket_section = match ticket {
        Some(t) if !t.trim().is_empty() => format!(
            "\n\nTicket / user story:\n{}",
            truncate_to_char_boundary(t, 1500)
        ),
        _ => String::new(),
    };

    Prompt {
        system: "You are a senior software engineer writing a GitHub pull request description.\n\
                 Given a git diff, produce a concise and informative PR description.\n\n\
                 Return a JSON object ONLY with this structure:\n\
                 {\"title\": \"imperative title under 72 chars\", \
                 \"summary\": [\"what changed and why — one sentence per bullet\"], \
                 \"test_plan\": [\"how to verify each change\"]}\n\n\
                 Rules:\n\
                 - title: imperative mood, under 72 chars, no period (e.g. \"Add JWT token refresh\")\n\
                 - summary: 2-4 bullets, each one sentence, focus on what and why\n\
                 - test_plan: 2-4 actionable steps a reviewer can follow to verify the change\n\
                 - Output the JSON object and nothing else."
            .to_string(),
        user: format!("Diff:\n{diff}{ticket_section}\n"),
    }
}

pub fn commit_message_prompt(diff: &str) -> Prompt {
    Prompt {
        system: "You are a senior software engineer writing a git commit message.\n\
                 Given a staged diff, produce a conventional commit message.\n\n\
                 Conventional commit format: <type>(<optional scope>): <short description>\n\
                 Types: feat, fix, docs, style, refactor, test, chore, perf\n\n\
                 Return a JSON object ONLY:\n\
                 {\"message\": \"feat(scope): short description\", \
                 \"body\": \"optional multi-line body explaining WHY (empty string if a one-liner is enough)\"}\n\n\
                 Rules:\n\
                 - message must be under 72 characters\n\
                 - Use imperative mood (\"add\" not \"added\", \"fix\" not \"fixed\")\n\
                 - scope is optional — use it when it meaningfully narrows the context\n\
                 - body explains motivation and context, not what the diff shows\n\
                 - Output the JSON object and nothing else."
            .to_string(),
        user: format!("Staged diff:\n{diff}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Severity;

    fn base<'a>(diff: &'a str) -> ReviewPromptInput<'a> {
        ReviewPromptInput {
            rulebooks: &[],
            max_rules_bytes: 4000,
            diff,
            context: "",
            languages: None,
            requirements: None,
            max_context_bytes: 2000,
            max_requirements_bytes: 2000,
        }
    }

    #[test]
    fn compliance_only_appears_with_requirements() {
        let without = review_prompt(&base("d"));
        assert!(!without.system.contains("compliance"));

        let mut input = base("d");
        input.requirements = Some("Must expire in 1 hour");
        let with = review_prompt(&input);
        assert!(with.system.contains("compliance"));
        assert!(with.system.contains("Must expire in 1 hour"));
    }

    #[test]
    fn blank_requirements_do_not_enable_compliance() {
        let mut input = base("d");
        input.requirements = Some("   \n ");
        assert!(!review_prompt(&input).system.contains("compliance"));
    }

    #[test]
    fn context_is_truncated_on_a_char_boundary() {
        let ctx = "é".repeat(100);
        let mut input = base("d");
        input.context = &ctx;
        input.max_context_bytes = 15;
        // The assertion is simply that building the prompt does not panic and
        // yields valid UTF-8 — slicing mid-codepoint would abort.
        let p = review_prompt(&input);
        assert!(p.user.contains('é'));
    }

    fn rb(id: &str, severity: Option<Severity>) -> Rulebook {
        Rulebook {
            id: id.into(),
            scope: vec![],
            severity,
            body: format!("- Rule from {id}."),
        }
    }

    /// The whole reason rule bodies live in the system half.
    #[test]
    fn the_system_prompt_is_identical_for_two_units_under_the_same_rules() {
        let books = [rb("api", Some(Severity::High))];
        let refs: Vec<&Rulebook> = books.iter().collect();

        let mut a = base("diff for unit A");
        a.rulebooks = &refs;
        a.context = "context for A";

        let mut b = base("diff for unit B");
        b.rulebooks = &refs;
        b.context = "totally different context for B";

        let (pa, pb) = (review_prompt(&a), review_prompt(&b));
        assert_eq!(
            pa.system, pb.system,
            "the cacheable prefix must not vary with per-unit data"
        );
        assert_ne!(pa.user, pb.user, "the per-unit half must still differ");
    }

    #[test]
    fn rule_bodies_and_their_severity_reach_the_system_prompt() {
        let books = [rb("api-conventions", Some(Severity::High))];
        let refs: Vec<&Rulebook> = books.iter().collect();
        let mut input = base("d");
        input.rulebooks = &refs;

        let p = review_prompt(&input);
        assert!(p.system.contains("Rule from api-conventions"));
        assert!(p.system.contains("api-conventions (high severity)"));
        assert!(
            p.system.contains("\"rule\""),
            "the model needs to be told how to attribute a finding"
        );
    }

    #[test]
    fn without_rule_sets_the_prompt_does_not_mention_them() {
        // Existing users have no `.diffmind/rules/`; their prompt must not grow
        // an empty section or an instruction about a field that cannot apply.
        let p = review_prompt(&base("d"));
        assert!(!p.system.contains("Project review rules"));
        assert!(!p.system.contains("\"rule\""));
    }

    #[test]
    fn an_oversized_rule_set_is_dropped_whole_rather_than_cut_in_half() {
        let books = [
            rb("small", None),
            Rulebook {
                id: "huge".into(),
                scope: vec![],
                severity: None,
                body: "x".repeat(5000),
            },
        ];
        let refs: Vec<&Rulebook> = books.iter().collect();
        let mut input = base("d");
        input.rulebooks = &refs;
        input.max_rules_bytes = 200;

        let p = review_prompt(&input);
        assert!(p.system.contains("Rule from small"));
        assert!(
            !p.system.contains("huge"),
            "half a rule reads as a complete rule that says something else"
        );
    }

    #[test]
    fn chatml_wraps_both_roles() {
        let p = Prompt {
            system: "S".into(),
            user: "U".into(),
        };
        let rendered = p.to_chatml();
        assert!(rendered.starts_with("<|im_start|>system\nS<|im_end|>"));
        assert!(rendered.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn languages_reach_the_system_prompt() {
        let mut input = base("d");
        input.languages = Some("Rust, Go");
        assert!(review_prompt(&input).system.contains("Rust, Go"));
    }
}
