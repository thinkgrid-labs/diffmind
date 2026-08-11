//! SARIF 2.1.0 output for GitHub Code Scanning.
//!
//! Uploading this to `github/codeql-action/upload-sarif` puts findings inline
//! on the PR diff with no bot account, no API token, and no comment spam — the
//! cheapest path from "diffmind runs in CI" to "reviewers actually see it".

use crate::types::{ReviewFinding, ReviewSummary, Severity, rule_description};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const SCHEMA: &str = "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json";

/// Map to SARIF's `level`. Code Scanning surfaces `error` as a blocking
/// annotation and `note` as informational.
fn level(severity: Severity) -> &'static str {
    match severity {
        Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// GitHub sorts and filters by `security-severity` (a CVSS-like number) for
/// rules tagged `security`, so give it a sane value rather than leaving alerts
/// unranked.
fn security_severity(severity: Severity) -> &'static str {
    match severity {
        Severity::High => "8.0",
        Severity::Medium => "5.0",
        Severity::Low => "2.0",
    }
}

pub fn to_sarif(summary: &ReviewSummary, tool_version: &str, model: &str) -> Value {
    // BTreeMap keeps the rule table in a stable order so the artifact does not
    // churn between runs on identical input.
    let mut rules: BTreeMap<String, &ReviewFinding> = BTreeMap::new();
    for f in &summary.findings {
        rules.entry(f.rule_id()).or_insert(f);
    }

    let rule_defs: Vec<Value> = rules
        .iter()
        .map(|(id, f)| {
            let description = rule_description(id)
                .map(str::to_string)
                .unwrap_or_else(|| f.issue.clone());
            json!({
                "id": id,
                "name": id,
                "shortDescription": { "text": truncate(&description, 120) },
                "fullDescription": { "text": description },
                "defaultConfiguration": { "level": level(f.severity) },
                "properties": {
                    "tags": [f.category.as_str()],
                    "security-severity": security_severity(f.severity),
                }
            })
        })
        .collect();

    let results: Vec<Value> = summary
        .findings
        .iter()
        .map(|f| {
            json!({
                "ruleId": f.rule_id(),
                "level": level(f.severity),
                "message": { "text": message_text(f) },
                "partialFingerprints": { "diffmindFingerprint/v1": f.fingerprint() },
                "properties": {
                    "confidence": f.confidence_or_default(),
                    "category": f.category.as_str(),
                },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": {
                            "uri": f.file,
                            "uriBaseId": "%SRCROOT%",
                        },
                        // SARIF requires startLine >= 1; a 0 makes GitHub reject
                        // the whole upload rather than skip the one result.
                        "region": { "startLine": f.line.max(1) }
                    }
                }]
            })
        })
        .collect();

    json!({
        "$schema": SCHEMA,
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "diffmind",
                    "version": tool_version,
                    "informationUri": "https://github.com/thinkgrid-labs/diffmind",
                    "semanticVersion": tool_version,
                    "rules": rule_defs,
                }
            },
            "properties": {
                "model": model,
                "positives": summary.positives,
                "suggestions": summary.suggestions,
            },
            "results": results,
        }]
    })
}

fn message_text(f: &ReviewFinding) -> String {
    if f.suggested_fix.trim().is_empty() {
        f.issue.clone()
    } else {
        format!("{}\n\nSuggested fix: {}", f.issue, f.suggested_fix)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max - 1).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, ReviewFinding};

    fn finding(sev: Severity, rule: &str, line: u32) -> ReviewFinding {
        ReviewFinding {
            file: "src/a.rs".into(),
            line,
            severity: sev,
            category: Category::Security,
            issue: "hardcoded secret".into(),
            suggested_fix: "use env vars".into(),
            confidence: Some(0.9),
            rule_id: Some(rule.into()),
            rule: None,
            unit_id: None,
        }
    }

    fn summary() -> ReviewSummary {
        ReviewSummary {
            findings: vec![
                finding(Severity::High, "DM001", 12),
                finding(Severity::Low, "DM002", 3),
            ],
            positives: vec!["clean error handling".into()],
            suggestions: vec![],
        }
    }

    #[test]
    fn produces_a_wellformed_run() {
        let v = to_sarif(&summary(), "0.8.0", "qwen2.5-coder-1.5b");
        assert_eq!(v["version"], "2.1.0");
        assert_eq!(v["runs"][0]["tool"]["driver"]["name"], "diffmind");
        assert_eq!(v["runs"][0]["tool"]["driver"]["version"], "0.8.0");
        assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn severity_maps_to_sarif_levels() {
        let v = to_sarif(&summary(), "0.8.0", "m");
        assert_eq!(v["runs"][0]["results"][0]["level"], "error");
        assert_eq!(v["runs"][0]["results"][1]["level"], "note");
    }

    #[test]
    fn every_rule_is_declared_exactly_once() {
        let mut s = summary();
        // Same rule twice must not produce a duplicate rule definition, which
        // GitHub rejects.
        s.findings.push(finding(Severity::High, "DM001", 40));
        let v = to_sarif(&s, "0.8.0", "m");
        let rules = v["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        let ids: Vec<_> = rules.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["DM001", "DM002"], "stable, sorted rule order");
    }

    #[test]
    fn start_line_is_never_zero() {
        let mut s = summary();
        s.findings[0].line = 0;
        let v = to_sarif(&s, "0.8.0", "m");
        assert_eq!(
            v["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]["startLine"],
            1,
            "a zero startLine makes GitHub reject the entire upload"
        );
    }

    #[test]
    fn fingerprints_let_github_track_an_alert_across_runs() {
        let v = to_sarif(&summary(), "0.8.0", "m");
        let fp = &v["runs"][0]["results"][0]["partialFingerprints"]["diffmindFingerprint/v1"];
        assert!(fp.is_string() && !fp.as_str().unwrap().is_empty());
    }

    #[test]
    fn message_includes_the_suggested_fix() {
        let v = to_sarif(&summary(), "0.8.0", "m");
        let msg = v["runs"][0]["results"][0]["message"]["text"]
            .as_str()
            .unwrap();
        assert!(msg.contains("hardcoded secret"));
        assert!(msg.contains("use env vars"));
    }

    #[test]
    fn security_severity_is_present_for_ranking() {
        let v = to_sarif(&summary(), "0.8.0", "m");
        let rules = v["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules[0]["properties"]["security-severity"], "8.0");
    }
}
