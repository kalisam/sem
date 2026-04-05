//! Contract verification: check that callers pass the correct number of
//! arguments to callees. Heuristic, not perfect, but catches obvious mismatches.

use std::path::Path;

use crate::model::entity::SemanticEntity;
use crate::parser::graph::EntityGraph;
use crate::parser::registry::ParserRegistry;

#[derive(Debug, Clone)]
pub struct ContractViolation {
    pub entity_name: String,
    pub file_path: String,
    pub expected_params: usize,
    pub caller_name: String,
    pub caller_file: String,
    pub actual_args: usize,
}

/// Verify function call contracts across the codebase.
///
/// For each `Calls` edge in the graph, extracts expected param count from
/// the callee's first line and actual arg count from the call site in the
/// caller's content. Flags mismatches.
///
/// If `target_file` is Some, only report violations for callees in that file.
pub fn verify_contracts(
    root: &Path,
    file_paths: &[String],
    registry: &ParserRegistry,
    target_file: Option<&str>,
) -> Vec<ContractViolation> {
    let graph = EntityGraph::build(root, file_paths, registry);

    // Build content map: entity_id -> content
    let mut content_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for fp in file_paths {
        let full = root.join(fp);
        let content = match std::fs::read_to_string(&full) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let plugin = match registry.get_plugin(fp) {
            Some(p) => p,
            None => continue,
        };
        for entity in plugin.extract_entities(&content, fp) {
            content_map.insert(entity.id.clone(), entity.content.clone());
        }
    }

    let mut violations = Vec::new();

    for edge in &graph.edges {
        if edge.ref_type != crate::parser::graph::RefType::Calls {
            continue;
        }

        let callee = match graph.entities.get(&edge.to_entity) {
            Some(e) => e,
            None => continue,
        };

        // Filter to target file if specified
        if let Some(tf) = target_file {
            if callee.file_path != tf {
                continue;
            }
        }

        // Only check functions/methods
        if !matches!(
            callee.entity_type.as_str(),
            "function" | "method" | "arrow_function"
        ) {
            continue;
        }

        let callee_content = match content_map.get(&edge.to_entity) {
            Some(c) => c,
            None => continue,
        };

        let caller = match graph.entities.get(&edge.from_entity) {
            Some(e) => e,
            None => continue,
        };

        let caller_content = match content_map.get(&edge.from_entity) {
            Some(c) => c,
            None => continue,
        };

        let expected = extract_param_count(callee_content);
        if expected == 0 {
            continue; // can't verify zero-param functions meaningfully
        }

        if let Some(actual) = count_call_args(caller_content, &callee.name) {
            if actual != expected {
                violations.push(ContractViolation {
                    entity_name: callee.name.clone(),
                    file_path: callee.file_path.clone(),
                    expected_params: expected,
                    caller_name: caller.name.clone(),
                    caller_file: caller.file_path.clone(),
                    actual_args: actual,
                });
            }
        }
    }

    violations
}

/// Like `verify_contracts`, but accepts a pre-built graph + entities to avoid
/// redundant work when the caller already has them cached.
pub fn verify_contracts_with_graph(
    graph: &EntityGraph,
    all_entities: &[SemanticEntity],
    target_file: Option<&str>,
) -> Vec<ContractViolation> {
    let content_map: std::collections::HashMap<String, String> = all_entities
        .iter()
        .map(|e| (e.id.clone(), e.content.clone()))
        .collect();

    let mut violations = Vec::new();

    for edge in &graph.edges {
        if edge.ref_type != crate::parser::graph::RefType::Calls {
            continue;
        }

        let callee = match graph.entities.get(&edge.to_entity) {
            Some(e) => e,
            None => continue,
        };

        if let Some(tf) = target_file {
            if callee.file_path != tf {
                continue;
            }
        }

        if !matches!(
            callee.entity_type.as_str(),
            "function" | "method" | "arrow_function"
        ) {
            continue;
        }

        let callee_content = match content_map.get(&edge.to_entity) {
            Some(c) => c,
            None => continue,
        };

        let caller = match graph.entities.get(&edge.from_entity) {
            Some(e) => e,
            None => continue,
        };

        let caller_content = match content_map.get(&edge.from_entity) {
            Some(c) => c,
            None => continue,
        };

        let expected = extract_param_count(callee_content);
        if expected == 0 {
            continue;
        }

        if let Some(actual) = count_call_args(caller_content, &callee.name) {
            if actual != expected {
                violations.push(ContractViolation {
                    entity_name: callee.name.clone(),
                    file_path: callee.file_path.clone(),
                    expected_params: expected,
                    caller_name: caller.name.clone(),
                    caller_file: caller.file_path.clone(),
                    actual_args: actual,
                });
            }
        }
    }

    violations
}

/// Extract param count from the first line of a function/method.
/// Looks for the pattern `name(param1, param2, ...)` and counts commas + 1.
fn extract_param_count(content: &str) -> usize {
    let first_line = content.lines().next().unwrap_or("");

    // Find the opening paren
    let open = match first_line.find('(') {
        Some(i) => i,
        None => return 0,
    };

    // Find matching close paren (handle nested parens)
    let after_open = &first_line[open + 1..];
    let close = match find_matching_paren(after_open) {
        Some(i) => i,
        None => return 0,
    };

    let params_str = after_open[..close].trim();
    if params_str.is_empty() {
        return 0;
    }

    // Count params by splitting on commas at depth 0
    count_top_level_commas(params_str) + 1
}

/// Count arguments at a call site: find `callee_name(...)` in content and count args.
fn count_call_args(content: &str, callee_name: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let name_bytes = callee_name.as_bytes();
    let mut search_start = 0;

    while let Some(rel_pos) = content[search_start..].find(callee_name) {
        let pos = search_start + rel_pos;
        let after = pos + name_bytes.len();

        // Check word boundary before
        let is_boundary = pos == 0 || {
            let prev = bytes[pos - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_'
        };

        // Check '(' follows
        if is_boundary && after < bytes.len() && bytes[after] == b'(' {
            let args_start = &content[after + 1..];
            if let Some(close) = find_matching_paren(args_start) {
                let args_str = args_start[..close].trim();
                if args_str.is_empty() {
                    return Some(0);
                }
                return Some(count_top_level_commas(args_str) + 1);
            }
        }

        search_start = pos + 1;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }

    None
}

/// Find the position of the matching close paren, handling nesting.
fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// Count commas at depth 0 (not inside nested parens/brackets).
fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0i32;
    let mut count = 0;
    for ch in s.chars() {
        match ch {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_param_count_basic() {
        assert_eq!(extract_param_count("function foo(a, b, c) {"), 3);
        assert_eq!(extract_param_count("function foo() {"), 0);
        assert_eq!(extract_param_count("def bar(self, x):"), 2);
        assert_eq!(extract_param_count("fn baz(a: i32) -> bool {"), 1);
    }

    #[test]
    fn test_extract_param_count_nested() {
        assert_eq!(extract_param_count("function foo(a, fn(x, y), c) {"), 3);
    }

    #[test]
    fn test_count_call_args() {
        assert_eq!(count_call_args("let x = foo(1, 2, 3);", "foo"), Some(3));
        assert_eq!(count_call_args("foo()", "foo"), Some(0));
        assert_eq!(count_call_args("bar(1)", "foo"), None);
        assert_eq!(count_call_args("foo(a, b)", "foo"), Some(2));
    }

    #[test]
    fn test_count_call_args_multibyte_utf8() {
        // Ensure no panic when content contains multi-byte UTF-8 characters before the call site
        assert_eq!(count_call_args("let café = foo(1, 2);", "foo"), Some(2));
        assert_eq!(count_call_args("let É = 1; bar(x)", "bar"), Some(1));
        assert_eq!(count_call_args("// 日本語コメント\nfoo(a, b, c)", "foo"), Some(3));
    }
}
