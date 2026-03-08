//! Hygiene — enforces coding standards at test time
//!
//! These tests scan the source tree for antipatterns that violate project
//! standards. Each has a budget (ideally zero). If you must add one, you
//! have to fix an existing one first — the budget never grows.
#![allow(clippy::absurd_extreme_comparisons)]

use std::fs;
use std::path::Path;

// Ratchet budgets: these reflect current production-code totals.
// Lower them as existing debt is removed; no category may grow.

// Panics — these crash the process, always budget zero.
const MAX_UNWRAP: usize = 0;
const MAX_EXPECT: usize = 1; // kernel.rs: inbound_rx.take().expect("kernel already started")
const MAX_PANIC: usize = 0;
const MAX_UNREACHABLE: usize = 0;
const MAX_TODO: usize = 0;
const MAX_UNIMPLEMENTED: usize = 0;

// Silent loss — discards errors without inspecting.
const MAX_SILENT_DISCARD: usize = 3;
const MAX_DOT_OK: usize = 0;

// Style / structure.
const MAX_ALLOW_DEAD_CODE: usize = 0;
const MAX_JSON_MACRO: usize = 0;

struct SourceFile {
    path: String,
    content: String,
}

/// Collect production `.rs` files (skip test files and target/).
fn source_files() -> Vec<SourceFile> {
    let mut files = Vec::new();
    collect_rs_files(Path::new("src"), &mut files);
    files
}

fn collect_rs_files(dir: &Path, out: &mut Vec<SourceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name == "target" || name == "tests" {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // Skip test files
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with("_test.rs") || name.ends_with(".test.rs") {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                out.push(SourceFile {
                    path: path.to_string_lossy().into_owned(),
                    content,
                });
            }
        }
    }
}

fn count_in_source(files: &[SourceFile], pattern: &str) -> usize {
    files
        .iter()
        .map(|file| {
            file.content
                .lines()
                .filter(|line| line.contains(pattern))
                .count()
        })
        .sum()
}

fn is_allowed_cfg_test(lines: &[&str], index: usize) -> bool {
    let Some(path_line) = lines.get(index + 1).map(|line| line.trim()) else {
        return false;
    };
    let Some(mod_line) = lines.get(index + 2).map(|line| line.trim()) else {
        return false;
    };
    path_line.starts_with("#[path = \"") && mod_line == "mod tests;"
}

// ── Crash budget ──

#[test]
fn unwrap_budget() {
    let files = source_files();
    let count = count_in_source(&files, ".unwrap()");
    assert!(
        count <= MAX_UNWRAP,
        ".unwrap() budget exceeded: found {count}, max {MAX_UNWRAP}."
    );
}

#[test]
fn expect_budget() {
    let files = source_files();
    let count = count_in_source(&files, ".expect(");
    assert!(
        count <= MAX_EXPECT,
        ".expect() budget exceeded: found {count}, max {MAX_EXPECT}."
    );
}

#[test]
fn panic_budget() {
    let files = source_files();
    let count = count_in_source(&files, "panic!(");
    assert!(
        count <= MAX_PANIC,
        "panic!() budget exceeded: found {count}, max {MAX_PANIC}."
    );
}

#[test]
fn unreachable_budget() {
    let files = source_files();
    let count = count_in_source(&files, "unreachable!(");
    assert!(
        count <= MAX_UNREACHABLE,
        "unreachable!() budget exceeded: found {count}, max {MAX_UNREACHABLE}."
    );
}

#[test]
fn todo_budget() {
    let files = source_files();
    let count = count_in_source(&files, "todo!(");
    assert!(
        count <= MAX_TODO,
        "todo!() budget exceeded: found {count}, max {MAX_TODO}."
    );
}

#[test]
fn unimplemented_budget() {
    let files = source_files();
    let count = count_in_source(&files, "unimplemented!(");
    assert!(
        count <= MAX_UNIMPLEMENTED,
        "unimplemented!() budget exceeded: found {count}, max {MAX_UNIMPLEMENTED}."
    );
}

// ── Silent error loss ──

#[test]
fn silent_discard_budget() {
    let files = source_files();
    let count = count_in_source(&files, "let _ =");
    assert!(
        count <= MAX_SILENT_DISCARD,
        "let _ = budget exceeded: found {count}, max {MAX_SILENT_DISCARD}."
    );
}

#[test]
fn dot_ok_budget() {
    let files = source_files();
    let count = count_in_source(&files, ".ok()");
    assert!(
        count <= MAX_DOT_OK,
        ".ok() budget exceeded: found {count}, max {MAX_DOT_OK}."
    );
}

// ── Style / structure ──

#[test]
fn allow_dead_code_budget() {
    let files = source_files();
    let count = count_in_source(&files, "#[allow(dead_code)]");
    assert!(
        count <= MAX_ALLOW_DEAD_CODE,
        "#[allow(dead_code)] budget exceeded: found {count}, max {MAX_ALLOW_DEAD_CODE}."
    );
}

#[test]
fn json_macro_budget() {
    let files = source_files();
    let count = count_in_source(&files, "json!");
    assert!(
        count <= MAX_JSON_MACRO,
        "json! budget exceeded: found {count}, max {MAX_JSON_MACRO}. Use typed structs or Value::from()."
    );
}

#[test]
fn cfg_test_attribute_form() {
    let files = source_files();
    let mut violations = 0;
    let mut locations = Vec::new();
    for file in &files {
        let lines: Vec<&str> = file.content.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() == "#[cfg(test)]" && !is_allowed_cfg_test(&lines, index) {
                violations += 1;
                locations.push(format!("{}:{}", file.path, index + 1));
            }
        }
    }
    assert!(
        violations == 0,
        "#[cfg(test)] must be followed by #[path = \"*_test.rs\"] + mod tests;. Violations at: {}",
        locations.join(", ")
    );
}
