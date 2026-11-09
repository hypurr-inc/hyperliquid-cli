//! `settings::env` is the one place environment variable names are spelled.
//!
//! A name written as a string literal anywhere else in `src/` is a second copy
//! that a rename or a new variable silently misses. Tests (everything from the
//! first `#[cfg(test)]` of a file) and comments are exempt.

use std::fs;
use std::path::{Path, PathBuf};

use hyperliquid_cli::settings::env;

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// The contents of every string literal in `source`, skipping comments.
fn string_literals(source: &str) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let mut literals = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '/' if chars.get(index + 1) == Some(&'/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '\'' if chars.get(index + 2) == Some(&'\'') => index += 3,
            '\'' if chars.get(index + 1) == Some(&'\\') && chars.get(index + 3) == Some(&'\'') => {
                index += 4;
            }
            '"' => {
                let mut literal = String::new();
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\' {
                        index += 1;
                    }
                    if let Some(c) = chars.get(index) {
                        literal.push(*c);
                    }
                    index += 1;
                }
                literals.push(literal);
                index += 1;
            }
            _ => index += 1,
        }
    }
    literals
}

fn mentions(literal: &str, name: &str) -> bool {
    literal.match_indices(name).any(|(start, _)| {
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let before = literal[..start].chars().next_back();
        let after = literal[start + name.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

#[test]
fn environment_variable_names_are_spelled_only_in_the_settings_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    // A bare word such as `CI` or `TERM` is ordinary prose; every other name has an underscore.
    let names: Vec<&str> = env::ALL
        .iter()
        .copied()
        .filter(|name| name.contains('_'))
        .collect();

    let mut offenders = Vec::new();
    for file in files {
        if file.ends_with("settings.rs") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        let production = text.split("#[cfg(test)]").next().unwrap();
        for literal in string_literals(production) {
            for name in &names {
                if mentions(&literal, name) {
                    offenders.push(format!(
                        "{}: {name} in {:?}",
                        file.strip_prefix(&root).unwrap().display(),
                        literal.chars().take(60).collect::<String>()
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "spell these through `settings::env`:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scanner_finds_names_in_literals_and_ignores_comments_and_words() {
    let source = "// \"HYPERLIQUID_FORMAT\"\nlet a = \"set HYPERLIQUID_FORMAT=json\";\nlet b = '\"';\nlet c = \"HYPERLIQUID_FORMATTED\";";
    let literals = string_literals(source);
    assert_eq!(literals.len(), 2, "{literals:?}");
    assert!(mentions(&literals[0], env::FORMAT));
    assert!(!mentions(&literals[1], env::FORMAT));
}
