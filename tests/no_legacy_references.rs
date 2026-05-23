//! Compile-time-ish check that the rename to `blueprint` is complete.
//!
//! Walks every tracked-extension file under the manifest dir and fails if any
//! line contains `plan-share`, `plan_share`, or `PLAN_SHARE`. The Rust compiler
//! catches most of the rename surface automatically (every renamed identifier
//! produces an unresolved reference); this test catches the remaining class —
//! string literals in URLs, paths, env-var names, help text, comments, docs,
//! and frontend JS that the compiler never inspects.
//!
//! Skipped: target/, .git/, node_modules/, Cargo.lock (transitive deps may
//! genuinely have those substrings in their names), and this file itself.

use std::fs;
use std::path::{Path, PathBuf};

const EXTENSIONS: &[&str] = &["rs", "toml", "md", "js", "html", "css", "json"];
// `.claude/` is per-developer harness state (permission allowlists, etc.) —
// historical strings there don't affect the binary's behavior, and forcing a
// rewrite would make this test non-reproducible across machines.
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".plan-share", ".claude"];
const SKIP_FILES: &[&str] = &["Cargo.lock", "no_legacy_references.rs"];
const NEEDLES: &[&str] = &["plan-share", "plan_share", "PLAN_SHARE"];

fn walk(dir: &Path, hits: &mut Vec<(PathBuf, usize, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if SKIP_DIRS.contains(&name) {
            continue;
        }
        if path.is_dir() {
            walk(&path, hits);
            continue;
        }
        if SKIP_FILES.contains(&name) {
            continue;
        }
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !EXTENSIONS.contains(&ext) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            for needle in NEEDLES {
                if line.contains(needle) {
                    hits.push((path.clone(), i + 1, line.to_string()));
                }
            }
        }
    }
}

#[test]
fn no_plan_share_references_remain() {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut hits = Vec::new();
    walk(Path::new(root), &mut hits);
    if !hits.is_empty() {
        eprintln!(
            "\n{} stale plan-share reference(s) — drop or rename:\n",
            hits.len()
        );
        for (p, ln, content) in &hits {
            let rel = p
                .strip_prefix(root)
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| p.display().to_string());
            eprintln!("  {rel}:{ln}: {}", content.trim());
        }
        panic!("{} stale references", hits.len());
    }
}
