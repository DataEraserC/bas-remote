//! The core stays a protocol layer.
//!
//! Everything that involves a machine-local decision — reading a file, writing
//! one, opening a browser, prompting a human, spawning a process, reading the
//! environment — belongs to `bas-api` (effects behind the C ABI) or `bas-cli`
//! (a human at a terminal). `bas-core` takes its inputs as arguments and
//! returns data.
//!
//! This test scans every `.rs` file of the crate and fails when one of those
//! primitives shows up, so the boundary cannot erode silently.

use std::fs;
use std::path::{Path, PathBuf};

/// Primitives that must never appear in `bas-core`.
///
/// Written as fragments so this file itself does not trip the scan.
const FORBIDDEN: &[&str] = &[
    "std::fs",
    "File::open",
    "File::create",
    "std::env",
    "std::process",
    "Command::new",
    "read_line",
    "println!",
    "eprintln!",
    "dirs::home_dir",
    "std::io::stdin",
];

/// Tokens allowed in specific files, with the reason they are needed there.
///
/// `println!` in a doc example or a test is not an effect on the protocol; the
/// scan only looks at production sources, so this list stays empty on purpose.
const ALLOWED: &[(&str, &str)] = &[];

/// Drop `//` line comments, ignoring `//` that lives inside a string literal
/// (the core carries full URLs, e.g. `https://…`).
fn strip_comments(line: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '/' && chars.get(index + 1) == Some(&'/') {
            return chars[..index].iter().collect();
        }
        index += 1;
    }
    line.to_string()
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn core_has_no_effects() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_sources(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        root.display()
    );

    let mut violations = Vec::new();
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .display()
            .to_string();
        // `#[cfg(test)]` modules may print and use the filesystem: they are
        // not part of the library that ships.
        let mut in_tests = false;
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("#[cfg(test)]") {
                in_tests = true;
            }
            if in_tests {
                continue;
            }
            let code = strip_comments(line);
            for token in FORBIDDEN {
                if !code.contains(token) {
                    continue;
                }
                if ALLOWED
                    .iter()
                    .any(|(path, allowed)| relative == *path && allowed == token)
                {
                    continue;
                }
                violations.push(format!("{relative}:{}: `{token}`", index + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "bas-core must stay a protocol layer: no filesystem, environment, \
         process or terminal access. Move these to bas-api or bas-cli:\n  {}",
        violations.join("\n  ")
    );
}
