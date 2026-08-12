//! Incremental JSON validity checking, used to constrain decoding.
//!
//! A 1.5B model asked for "a JSON object ONLY" answers with prose preambles,
//! trailing commentary, unbalanced braces and truncated strings often enough
//! that the CLI had to count unparseable chunks and tell users to try a bigger
//! model. Rather than parse-and-hope, the sampler consults this state machine
//! before committing each token: candidates that would make the output
//! unparseable are skipped in favour of the next-most-likely valid token.
//!
//! The machine answers two questions about the text generated so far:
//!   - could this still become valid JSON?  (`push` returns `Ok`)
//!   - is it valid JSON right now?          (`is_complete`)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Object,
    Array,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A value must appear next — at the start of the document, or after a
    /// `:`. Repairable: a `null` can be supplied.
    Value,
    /// A value, or `]` closing an empty array.
    ValueOrEnd,
    /// A value after a `,` inside an array. *Not* repairable: the comma is
    /// already committed, and there is no way to express an empty slot.
    ValueAfterComma,
    /// A quoted key, or `}` closing an empty object.
    KeyOrEnd,
    /// A quoted key after a `,` inside an object. Not repairable, as above.
    KeyAfterComma,
    /// The `:` between a key and its value.
    Colon,
    /// `,` for another element, or the container's closing bracket.
    CommaOrEnd,
    /// The top-level value is closed; only trailing whitespace may follow.
    Done,
}

impl Expect {
    /// States in which a fresh value may begin.
    fn accepts_value(self) -> bool {
        matches!(
            self,
            Expect::Value | Expect::ValueOrEnd | Expect::ValueAfterComma
        )
    }

    /// States in which an object key may begin.
    fn accepts_key(self) -> bool {
        matches!(self, Expect::KeyOrEnd | Expect::KeyAfterComma)
    }
}

/// Rejection reason. Callers only care that it failed, but naming the cases
/// keeps the state machine honest and makes test failures readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonError {
    UnexpectedChar(char),
    TrailingContent,
    BadLiteral(String),
    TooDeep,
}

/// Guards against a pathological model looping `[[[[[…`.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone)]
pub struct JsonPrefix {
    stack: Vec<Ctx>,
    expect: Expect,
    in_string: bool,
    escape: bool,
    /// Remaining hex digits expected in a `\uXXXX` escape.
    unicode_left: u8,
    /// Whether the string currently open is an object key.
    string_is_key: bool,
    /// Accumulates a number or `true`/`false`/`null` until a delimiter arrives.
    partial: String,
}

impl Default for JsonPrefix {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonPrefix {
    /// Accepts any JSON value at the top level.
    pub fn new() -> Self {
        JsonPrefix {
            stack: Vec::new(),
            expect: Expect::Value,
            in_string: false,
            escape: false,
            unicode_left: 0,
            string_is_key: false,
            partial: String::new(),
        }
    }

    /// Feed one character. `Err` means no continuation can rescue this text.
    pub fn push(&mut self, c: char) -> Result<(), JsonError> {
        if self.in_string {
            return self.push_in_string(c);
        }

        if !self.partial.is_empty() {
            if continues_literal(c) {
                let mut candidate = self.partial.clone();
                candidate.push(c);
                if !literal_prefix_ok(&candidate) {
                    return Err(JsonError::BadLiteral(candidate));
                }
                self.partial.push(c);
                return Ok(());
            }
            // A delimiter ends the literal; it must be complete to stand alone.
            self.finish_literal()?;
            // Fall through and reprocess `c` as a structural character.
        }

        if c.is_whitespace() {
            return Ok(());
        }

        match c {
            '"' if self.expect.accepts_value() => {
                self.in_string = true;
                self.string_is_key = false;
                Ok(())
            }
            '"' if self.expect.accepts_key() => {
                self.in_string = true;
                self.string_is_key = true;
                Ok(())
            }
            '"' => Err(JsonError::UnexpectedChar(c)),
            '{' => self.open(Ctx::Object, c),
            '[' => self.open(Ctx::Array, c),
            '}' => {
                let closable = self.expect == Expect::KeyOrEnd
                    || (self.expect == Expect::CommaOrEnd
                        && self.stack.last() == Some(&Ctx::Object));
                if !closable {
                    return Err(JsonError::UnexpectedChar(c));
                }
                self.stack.pop();
                self.complete_value();
                Ok(())
            }
            ']' => {
                let closable = self.expect == Expect::ValueOrEnd
                    || (self.expect == Expect::CommaOrEnd
                        && self.stack.last() == Some(&Ctx::Array));
                if !closable {
                    return Err(JsonError::UnexpectedChar(c));
                }
                self.stack.pop();
                self.complete_value();
                Ok(())
            }
            ':' => {
                if self.expect == Expect::Colon {
                    self.expect = Expect::Value;
                    Ok(())
                } else {
                    Err(JsonError::UnexpectedChar(c))
                }
            }
            ',' => {
                if self.expect != Expect::CommaOrEnd {
                    return Err(JsonError::UnexpectedChar(c));
                }
                self.expect = match self.stack.last() {
                    Some(Ctx::Object) => Expect::KeyAfterComma,
                    Some(Ctx::Array) => Expect::ValueAfterComma,
                    // A comma at the top level means a second document.
                    None => return Err(JsonError::TrailingContent),
                };
                Ok(())
            }
            _ => {
                if !self.expect.accepts_value() {
                    return Err(JsonError::UnexpectedChar(c));
                }
                if !continues_literal(c) {
                    return Err(JsonError::UnexpectedChar(c));
                }
                let candidate = c.to_string();
                if !literal_prefix_ok(&candidate) {
                    return Err(JsonError::BadLiteral(candidate));
                }
                self.partial = candidate;
                Ok(())
            }
        }
    }

    fn open(&mut self, ctx: Ctx, c: char) -> Result<(), JsonError> {
        if !self.expect.accepts_value() {
            return Err(JsonError::UnexpectedChar(c));
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(JsonError::TooDeep);
        }
        self.stack.push(ctx);
        self.expect = match ctx {
            Ctx::Object => Expect::KeyOrEnd,
            Ctx::Array => Expect::ValueOrEnd,
        };
        Ok(())
    }

    fn push_in_string(&mut self, c: char) -> Result<(), JsonError> {
        if self.unicode_left > 0 {
            if !c.is_ascii_hexdigit() {
                return Err(JsonError::UnexpectedChar(c));
            }
            self.unicode_left -= 1;
            return Ok(());
        }
        if self.escape {
            self.escape = false;
            match c {
                '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => Ok(()),
                'u' => {
                    self.unicode_left = 4;
                    Ok(())
                }
                _ => Err(JsonError::UnexpectedChar(c)),
            }
        } else {
            match c {
                '\\' => {
                    self.escape = true;
                    Ok(())
                }
                '"' => {
                    self.in_string = false;
                    if self.string_is_key {
                        self.expect = Expect::Colon;
                    } else {
                        self.complete_value();
                    }
                    Ok(())
                }
                // Raw control characters are illegal inside a JSON string.
                c if (c as u32) < 0x20 => Err(JsonError::UnexpectedChar(c)),
                _ => Ok(()),
            }
        }
    }

    fn complete_value(&mut self) {
        self.expect = if self.stack.is_empty() {
            Expect::Done
        } else {
            Expect::CommaOrEnd
        };
    }

    fn finish_literal(&mut self) -> Result<(), JsonError> {
        let lit = std::mem::take(&mut self.partial);
        if !literal_complete(&lit) {
            return Err(JsonError::BadLiteral(lit));
        }
        self.complete_value();
        Ok(())
    }

    pub fn push_str(&mut self, s: &str) -> Result<(), JsonError> {
        for c in s.chars() {
            self.push(c)?;
        }
        Ok(())
    }

    /// Would `s` keep this text a viable JSON prefix? Non-mutating, for
    /// speculatively testing sampler candidates.
    pub fn accepts(&self, s: &str) -> bool {
        let mut probe = self.clone();
        probe.push_str(s).is_ok()
    }

    /// True when the text so far is already a complete, valid JSON document.
    pub fn is_complete(&self) -> bool {
        if self.in_string || !self.stack.is_empty() {
            return false;
        }
        if !self.partial.is_empty() {
            return literal_complete(&self.partial);
        }
        self.expect == Expect::Done
    }

    /// Whether [`closing_suffix`] would succeed, without building the string.
    ///
    /// Called once per generated token to remember the last point the output
    /// could have been closed cleanly, so a run that stops somewhere
    /// unrepairable can be rewound to it.
    pub fn is_closable(&self) -> bool {
        if !self.partial.is_empty() && !literal_complete(&self.partial) {
            return false;
        }
        let effective = if self.in_string {
            if self.string_is_key {
                Expect::Colon
            } else {
                Expect::CommaOrEnd
            }
        } else if !self.partial.is_empty() {
            Expect::CommaOrEnd
        } else {
            self.expect
        };
        !matches!(effective, Expect::ValueAfterComma | Expect::KeyAfterComma)
    }

    /// The shortest suffix that would close everything still open.
    ///
    /// Lets a run that hits the token cap mid-object be salvaged into parseable
    /// JSON instead of thrown away — the findings already emitted are usually
    /// the useful ones.
    pub fn closing_suffix(&self) -> Option<String> {
        // Nothing has been produced yet — there is nothing to salvage.
        if self.stack.is_empty() && self.partial.is_empty() && !self.in_string {
            return (self.expect == Expect::Done).then(String::new);
        }

        let mut out = String::new();
        let mut expect = self.expect;

        if self.in_string {
            // An unterminated escape would make the closing quote part of the
            // escape sequence, so neutralise it first.
            if self.escape {
                out.push('\\');
            } else if self.unicode_left > 0 {
                for _ in 0..self.unicode_left {
                    out.push('0');
                }
            }
            out.push('"');
            expect = if self.string_is_key {
                Expect::Colon
            } else {
                Expect::CommaOrEnd
            };
        } else if !self.partial.is_empty() {
            if !literal_complete(&self.partial) {
                // A half-written literal cannot be repaired without inventing data.
                return None;
            }
            // A complete literal that has not yet met its delimiter still
            // closes the value it belongs to.
            expect = Expect::CommaOrEnd;
        }

        match expect {
            // A key with no value yet: supply one so its object can close.
            Expect::Colon => out.push_str(":null"),
            // Immediately after `:` (or at the very start of a nested value).
            Expect::Value => out.push_str("null"),
            // A comma is already committed and an empty slot is not
            // expressible, so this cannot be repaired from here.
            Expect::ValueAfterComma | Expect::KeyAfterComma => return None,
            // Empty container, closed value, or finished document — the
            // brackets below are all that is needed.
            Expect::ValueOrEnd | Expect::KeyOrEnd | Expect::CommaOrEnd | Expect::Done => {}
        }

        for ctx in self.stack.iter().rev() {
            out.push(match ctx {
                Ctx::Object => '}',
                Ctx::Array => ']',
            });
        }

        Some(out)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}

/// Characters that can appear inside a number or a bare keyword.
fn continues_literal(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E') || c.is_ascii_alphabetic()
}

/// Could `s` grow into `true`, `false`, `null`, or a valid number?
fn literal_prefix_ok(s: &str) -> bool {
    for kw in ["true", "false", "null"] {
        if kw.starts_with(s) {
            return true;
        }
    }
    number_prefix_ok(s)
}

fn literal_complete(s: &str) -> bool {
    matches!(s, "true" | "false" | "null") || number_complete(s)
}

#[derive(PartialEq, Clone, Copy)]
enum Num {
    Start,
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpDigits,
}

fn number_state(s: &str) -> Option<Num> {
    let mut st = Num::Start;
    for c in s.chars() {
        st = match (st, c) {
            (Num::Start, '-') => Num::Minus,
            (Num::Start, '0') | (Num::Minus, '0') => Num::Zero,
            (Num::Start, '1'..='9') | (Num::Minus, '1'..='9') => Num::Int,
            (Num::Int, '0'..='9') => Num::Int,
            // JSON forbids leading zeros: `01` is two tokens, not a number.
            (Num::Zero, '.') | (Num::Int, '.') => Num::Dot,
            (Num::Dot, '0'..='9') | (Num::Frac, '0'..='9') => Num::Frac,
            (Num::Zero, 'e' | 'E') | (Num::Int, 'e' | 'E') | (Num::Frac, 'e' | 'E') => Num::Exp,
            (Num::Exp, '+' | '-') => Num::ExpSign,
            (Num::Exp, '0'..='9') | (Num::ExpSign, '0'..='9') | (Num::ExpDigits, '0'..='9') => {
                Num::ExpDigits
            }
            _ => return None,
        };
    }
    Some(st)
}

fn number_prefix_ok(s: &str) -> bool {
    number_state(s).is_some()
}

fn number_complete(s: &str) -> bool {
    matches!(
        number_state(s),
        Some(Num::Zero | Num::Int | Num::Frac | Num::ExpDigits)
    )
}

/// Salvage the longest valid JSON document from a truncated generation.
///
/// `text` is a viable JSON prefix that ran out of tokens. If it can be closed
/// where it stands, it is. Otherwise it is rewound to `last_closable_len` — the
/// most recent point at which closing was possible — and closed there. That
/// rescues the common case of stopping immediately after a comma, which cannot
/// be closed in place (an empty array slot is not expressible) but is perfectly
/// recoverable by dropping the half-started element.
pub fn repair_truncated(text: &str, last_closable_len: Option<usize>) -> Option<String> {
    let mut state = JsonPrefix::new();
    if state.push_str(text).is_ok()
        && let Some(suffix) = state.closing_suffix()
    {
        return Some(format!("{text}{suffix}"));
    }

    let len = last_closable_len?;
    if len > text.len() || !text.is_char_boundary(len) {
        return None;
    }
    let truncated = &text[..len];
    let mut state = JsonPrefix::new();
    state.push_str(truncated).ok()?;
    let suffix = state.closing_suffix()?;
    Some(format!("{truncated}{suffix}"))
}

/// Extract the first complete JSON value embedded in `text`.
///
/// The last line of defence for backends whose decoding cannot be constrained
/// (a remote endpoint that ignored `response_format`). Unlike the old
/// `find('{') .. rfind('}')` slice, this tracks string literals, so a `}`
/// inside an `issue` string no longer truncates the object.
pub fn extract_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    for (start, _) in text.char_indices() {
        if bytes[start] != b'{' && bytes[start] != b'[' {
            continue;
        }
        let mut state = JsonPrefix::new();
        let mut rejected = false;
        for (offset, c) in text[start..].char_indices() {
            if state.push(c).is_err() {
                rejected = true;
                break;
            }
            if state.is_complete() {
                return Some(&text[start..start + offset + c.len_utf8()]);
            }
        }
        // Why the two endings are not the same thing. A *rejected* opener was
        // never JSON — a brace in prose, `Note: {see below}` — and says nothing
        // about what follows, so keep looking. An opener that merely ran out of
        // text is genuinely unterminated, and every later opener is nested
        // inside it, so none of them can close either: stop rather than rescan
        // the tail once per brace.
        //
        // Conflating the two cost a whole unit whenever a model wrote a brace
        // before its answer, which is exactly the habit the constrained decoder
        // exists to correct — and the backends that cannot be constrained, the
        // remote ones, are the only callers that reach here.
        if !rejected {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts_all(s: &str) -> bool {
        let mut p = JsonPrefix::new();
        p.push_str(s).is_ok()
    }

    fn complete(s: &str) -> bool {
        let mut p = JsonPrefix::new();
        p.push_str(s).is_ok() && p.is_complete()
    }

    #[test]
    fn accepts_valid_documents() {
        assert!(complete(r#"{"a": 1}"#));
        assert!(complete(r#"{"findings": [], "positives": ["ok"]}"#));
        assert!(complete(r#"[{"a": -1.5e10}, true, null]"#));
        assert!(complete(r#"{"a": "esc \" and ÿ"}"#));
    }

    #[test]
    fn rejects_structural_mistakes() {
        assert!(!accepts_all(r#"{"a": 1,}"#), "trailing comma in object");
        assert!(!accepts_all(r#"[1,]"#), "trailing comma in array");
        assert!(!accepts_all(r#"{a: 1}"#), "unquoted key");
        assert!(!accepts_all(r#"{"a" 1}"#), "missing colon");
        assert!(!accepts_all(r#"{"a": 1}}"#), "extra closer");
        assert!(!accepts_all(r#"{"a": 01}"#), "leading zero");
        assert!(
            !accepts_all("{\"a\": \"raw\nnewline\"}"),
            "control char in string"
        );
    }

    #[test]
    fn rejects_a_prose_preamble() {
        // The single most common small-model failure.
        let mut p = JsonPrefix::new();
        assert!(p.push_str("Here is the review: ").is_err());
    }

    #[test]
    fn incomplete_prefixes_are_accepted_but_not_complete() {
        for s in [r#"{"#, r#"{"a"#, r#"{"a":"#, r#"{"a": 1"#, r#"{"a": [1, "#] {
            assert!(accepts_all(s), "{s:?} should still be viable");
            assert!(!complete(s), "{s:?} should not be complete");
        }
    }

    #[test]
    fn number_completeness_is_lazy() {
        // "1" is a value, but "1." is not — the sampler must not stop on it.
        assert!(complete("1"));
        assert!(!complete("1."));
        assert!(accepts_all("1."));
        assert!(complete("1.5"));
        assert!(!complete("-"));
    }

    #[test]
    fn accepts_probes_without_mutating() {
        let mut p = JsonPrefix::new();
        p.push_str(r#"{"a""#).unwrap();
        assert!(p.accepts(":"));
        assert!(!p.accepts(","));
        // The probe must not have advanced the real state.
        assert!(p.accepts(":"));
    }

    #[test]
    fn closing_suffix_salvages_a_truncated_object() {
        let mut p = JsonPrefix::new();
        p.push_str(r#"{"findings": [{"file": "a.rs", "line": 3"#)
            .unwrap();
        let suffix = p.closing_suffix().expect("should be closable");
        let repaired = format!(r#"{{"findings": [{{"file": "a.rs", "line": 3{suffix}"#);
        assert!(complete(&repaired), "repaired: {repaired}");
        assert!(serde_json::from_str::<serde_json::Value>(&repaired).is_ok());
    }

    #[test]
    fn closing_suffix_terminates_an_open_string() {
        let mut p = JsonPrefix::new();
        p.push_str(r#"{"findings": [{"issue": "it broke"#).unwrap();
        let suffix = p.closing_suffix().unwrap();
        let repaired = format!(r#"{{"findings": [{{"issue": "it broke{suffix}"#);
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "{repaired}"
        );
    }

    #[test]
    fn closing_suffix_supplies_a_value_for_a_dangling_key() {
        let mut p = JsonPrefix::new();
        p.push_str(r#"{"a": 1, "b":"#).unwrap();
        let suffix = p.closing_suffix().unwrap();
        let repaired = format!(r#"{{"a": 1, "b":{suffix}"#);
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "{repaired}"
        );
    }

    #[test]
    fn closing_suffix_refuses_an_unrepairable_dangling_comma() {
        let mut p = JsonPrefix::new();
        p.push_str(r#"{"a": 1,"#).unwrap();
        assert!(p.closing_suffix().is_none());
    }

    #[test]
    fn is_closable_agrees_with_closing_suffix() {
        for s in [
            r#"{"a": 1"#,
            r#"{"a": "text"#,
            r#"{"a": 1,"#,
            r#"["#,
            r#"[1,"#,
            r#"{"a""#,
            r#"{"a": 1.5"#,
            r#"{"a": 1."#,
        ] {
            let mut p = JsonPrefix::new();
            p.push_str(s).unwrap();
            assert_eq!(
                p.is_closable(),
                p.closing_suffix().is_some(),
                "disagreement on {s:?}"
            );
        }
    }

    #[test]
    fn repair_rewinds_past_a_dangling_comma() {
        // The exact shape that made a whole chunk unparseable: generation ran
        // out of tokens immediately after a comma, which cannot be closed where
        // it stands because an empty array slot is not expressible.
        let full = r#"{"findings": [{"file": "a.rs", "line": 3, "severity": "high", "category": "quality", "issue": "x", "suggested_fix": "y"}],"#;

        // Replay to find the last point that was closable, as generate() does.
        let mut state = JsonPrefix::new();
        let mut last_closable = None;
        for (i, c) in full.char_indices() {
            state.push(c).unwrap();
            if state.is_closable() {
                last_closable = Some(i + c.len_utf8());
            }
        }
        assert!(
            state.closing_suffix().is_none(),
            "precondition: unclosable in place"
        );

        let repaired = repair_truncated(full, last_closable).expect("should be salvageable");
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("repaired output must parse");
        assert_eq!(
            parsed["findings"].as_array().unwrap().len(),
            1,
            "the finding that was already emitted must survive the rewind"
        );
    }

    #[test]
    fn repair_closes_in_place_when_it_can() {
        let text = r#"{"findings": [{"file": "a.rs""#;
        let repaired = repair_truncated(text, None).expect("closable in place");
        assert!(
            serde_json::from_str::<serde_json::Value>(&repaired).is_ok(),
            "{repaired}"
        );
    }

    #[test]
    fn repair_gives_up_rather_than_invent_data() {
        // A half-written number with nothing closable behind it.
        assert!(repair_truncated("-", None).is_none());
    }

    #[test]
    fn extract_json_survives_braces_inside_strings() {
        // The old find('{')..rfind('}') slice truncated here.
        let text = r#"blah {"issue": "use ${x} not {y}"} trailing"#;
        let extracted = extract_json(text).unwrap();
        assert_eq!(extracted, r#"{"issue": "use ${x} not {y}"}"#);
        assert!(serde_json::from_str::<serde_json::Value>(extracted).is_ok());
    }

    #[test]
    fn extract_json_ignores_trailing_commentary() {
        let text = "{\"a\": 1}\n\nI hope this helps! Let me know {if} you need more.";
        assert_eq!(extract_json(text).unwrap(), r#"{"a": 1}"#);
    }

    #[test]
    fn extract_json_returns_none_when_unterminated() {
        assert!(extract_json(r#"{"a": [1, 2"#).is_none());
        // Nothing after an unterminated opener can close it either — the inner
        // brackets are inside it.
        assert!(extract_json(r#"{"a": {"b": 1"#).is_none());
    }

    /// A brace in the prose before the answer used to lose the answer.
    ///
    /// The scan tried the first `{`, found it was not JSON, and gave up instead
    /// of looking further along — so the unit was counted unparseable and its
    /// findings thrown away. Only reachable on backends whose decoding cannot be
    /// constrained, which is precisely where a stray preamble is likely.
    #[test]
    fn extract_json_skips_a_brace_that_is_only_prose() {
        let text = "Note: {see below}\n{\"findings\": [], \"positives\": [\"ok\"]}";
        assert_eq!(
            extract_json(text),
            Some(r#"{"findings": [], "positives": ["ok"]}"#)
        );

        // Several false starts, and a bracket rather than a brace.
        assert_eq!(
            extract_json("[not json] {also not} finally: [1, 2]"),
            Some("[1, 2]")
        );
    }

    /// The whole pipeline, not just the extractor: a preamble containing a brace
    /// must still yield the findings the model actually reported.
    #[test]
    fn a_braced_preamble_does_not_lose_the_findings() {
        let response = "Looking at the diff {specifically auth.rs}, here is the review:\n\
             {\"findings\":[{\"file\":\"a.rs\",\"line\":1,\"severity\":\"high\",\
             \"category\":\"security\",\"issue\":\"boom\",\"suggested_fix\":\"f\"}],\
             \"positives\":[],\"suggestions\":[]}";
        let summary = crate::analyzer::parse_review_response(response)
            .expect("a brace in the preamble must not cost the whole unit");
        assert_eq!(summary.findings.len(), 1);
        assert_eq!(summary.findings[0].issue, "boom");
    }
}
