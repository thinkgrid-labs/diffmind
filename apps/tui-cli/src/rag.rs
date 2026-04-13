use crate::indexer::{COMMON_KEYWORDS, SymbolIndex};
use regex::Regex;

const MAX_CONTEXT_BYTES: usize = 3000;

pub fn get_rag_context(diff: &str, index: &SymbolIndex) -> Option<String> {
    let found_symbols = extract_symbols_from_diff(diff, index);
    if found_symbols.is_empty() {
        return None;
    }

    let mut symbols_list: Vec<String> = found_symbols.into_iter().collect();
    symbols_list.truncate(10);

    let context = build_context_string(&symbols_list, index);
    if context.is_empty() {
        None
    } else {
        Some(context)
    }
}

fn extract_symbols_from_diff(diff: &str, index: &SymbolIndex) -> std::collections::HashSet<String> {
    let mut found = std::collections::HashSet::new();
    let re = Regex::new(r"[a-zA-Z0-9_$]+").unwrap();

    for line in diff.lines() {
        if !line.starts_with("+") || line.starts_with("+++") {
            continue;
        }
        for mat in re.find_iter(line) {
            let word = mat.as_str();
            if index.symbols.contains_key(word) && !COMMON_KEYWORDS.contains(word) {
                found.insert(word.to_string());
            }
        }
    }
    found
}

fn build_context_string(symbols: &[String], index: &SymbolIndex) -> String {
    let mut result = String::new();
    for name in symbols {
        if let Some(def) = index.symbols.get(name) {
            let entry = format!(
                "\n--- Arch Reference: {} (from {}) ---\n{}\n",
                name, def.file, def.snippet
            );
            if result.len() + entry.len() > MAX_CONTEXT_BYTES {
                break;
            }
            result.push_str(&entry);
        }
    }
    result
}
