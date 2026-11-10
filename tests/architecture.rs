//! The layer rules, enforced on the source: the layer rule for `commands`, and
//! that nothing prints but through `output::sink`.
//!
//! A command is a module under `src/commands/` that implements `CliCommand`.
//! Commands are leaves, not a hub:
//!
//! 1. no file under `src/commands/` (other than `commands/mod.rs`, the
//!    registry wiring that names every command) imports another command's
//!    module; what two commands share lives in `commands/common/`,
//!    `exchange/` or a domain module under the crate root;
//! 2. no file outside `src/commands/` imports `commands`, except the binary
//!    crate (`main.rs` and the modules it declares), which is the application
//!    edge and reaches the library as `hyperliquid_cli`. The contract registry
//!    (`command_registry.rs`) is plain data: `commands::registry` builds it out
//!    of the entries, and `schema` and `mcp` receive it from there.
//!
//! The command list is derived from the `impl CliCommand for` blocks in source,
//! and every path is resolved (`crate::`, `super::`, `self::`, `use` groups),
//! so a re-spelled import does not slip through. Tests are scanned like code.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use support::rust_files;

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// `source` without comments, and with the inside of string and char literals
/// blanked, so a path written in prose or a message is not an import.
fn code_only(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '/' if chars.get(index + 1) == Some(&'/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                    index += 1;
                }
                index += 2;
            }
            'r' if matches!(chars.get(index + 1), Some('"' | '#'))
                && !out.ends_with(|c: char| c.is_alphanumeric() || c == '_') =>
            {
                let mut hashes = 0;
                let mut cursor = index + 1;
                while chars.get(cursor) == Some(&'#') {
                    hashes += 1;
                    cursor += 1;
                }
                if chars.get(cursor) != Some(&'"') {
                    out.push('r');
                    index += 1;
                    continue;
                }
                cursor += 1;
                let closing: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                while cursor < chars.len()
                    && !chars[cursor..].starts_with(&closing.chars().collect::<Vec<_>>())
                {
                    cursor += 1;
                }
                index = cursor + closing.len();
                out.push_str("\"\"");
            }
            '\'' if chars.get(index + 2) == Some(&'\'') => index += 3,
            '\'' if chars.get(index + 1) == Some(&'\\') && chars.get(index + 3) == Some(&'\'') => {
                index += 4;
            }
            '"' => {
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\' {
                        index += 1;
                    }
                    index += 1;
                }
                index += 1;
                out.push_str("\"\"");
            }
            c => {
                out.push(c);
                index += 1;
            }
        }
    }
    out
}

/// A use tree expanded to the plain paths it names:
/// `a::{b, c::{d, e}}` is `a::b`, `a::c::d` and `a::c::e`.
fn expand_use_tree(tree: &str) -> Vec<String> {
    let tree: String = tree.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(open) = tree.find('{') else {
        return vec![tree];
    };
    let mut depth = 0;
    let mut close = tree.len();
    for (offset, c) in tree[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = open + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    let (prefix, inner, rest) = (&tree[..open], &tree[open + 1..close], &tree[close + 1..]);
    let mut branches = vec![String::new()];
    let mut depth = 0;
    for c in inner.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                branches.push(String::new());
                continue;
            }
            _ => {}
        }
        branches.last_mut().unwrap().push(c);
    }
    branches
        .iter()
        .filter(|branch| !branch.is_empty())
        .flat_map(|branch| expand_use_tree(&format!("{prefix}{branch}{rest}")))
        .collect()
}

/// `tree` without its `as name` renames, keeping the separators after a name.
fn without_aliases(tree: &str) -> String {
    let mut out = String::new();
    let mut tokens = tree.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "as" {
            if let Some(alias) = tokens.next() {
                out.extend(alias.chars().skip_while(|c| is_identifier(*c)));
            }
        } else {
            out.push_str(token);
        }
    }
    out
}

fn is_identifier(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every path in `code` that starts at `crate`, `super`, `self` or
/// `hyperliquid_cli`: the paths of `use` trees, expanded, and the ones written
/// inline.
fn rooted_paths(code: &str) -> Vec<Vec<String>> {
    let mut texts: Vec<String> = Vec::new();
    let mut rest = code;
    while let Some(at) = rest.find("use ") {
        let boundary = !rest[..at].ends_with(is_identifier);
        let after = &rest[at + 4..];
        if boundary && let Some(end) = after.find(';') {
            texts.extend(expand_use_tree(&without_aliases(&after[..end])));
            rest = &after[end..];
        } else {
            rest = after;
        }
    }
    let chars: Vec<char> = code.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let starts_word = index == 0 || !is_identifier(chars[index - 1]) && chars[index - 1] != ':';
        if starts_word && is_identifier(chars[index]) {
            let mut end = index;
            while end < chars.len() && (is_identifier(chars[end]) || chars[end] == ':') {
                end += 1;
            }
            texts.push(chars[index..end].iter().collect());
            index = end;
        } else {
            index += 1;
        }
    }
    texts
        .into_iter()
        .map(|text| {
            text.split("::")
                .filter(|segment| !segment.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|path| {
            path.len() > 1
                && matches!(
                    path[0].as_str(),
                    "crate" | "super" | "self" | "hyperliquid_cli"
                )
        })
        .collect()
}

/// `path` as an absolute list of segments, starting at `crate` (or
/// `hyperliquid_cli`), given the module `here` the code is in.
fn resolve(path: &[String], here: &[String]) -> Vec<String> {
    let mut module = here.to_vec();
    let mut index = 0;
    match path[0].as_str() {
        "crate" | "hyperliquid_cli" => {
            return path
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if i == 0 {
                        "crate".to_string()
                    } else {
                        s.clone()
                    }
                })
                .collect();
        }
        "self" => index = 1,
        _ => {
            while path.get(index).is_some_and(|segment| segment == "super") {
                module.pop();
                index += 1;
            }
        }
    }
    let mut resolved = vec!["crate".to_string()];
    resolved.extend(module);
    resolved.extend(path[index..].iter().cloned());
    resolved
}

/// The module a file declares, as segments below `crate`.
fn module_of(file: &Path) -> Vec<String> {
    let relative = file.strip_prefix(src_dir()).unwrap().with_extension("");
    let mut segments: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if matches!(
        segments.last().map(String::as_str),
        Some("mod" | "main" | "lib")
    ) {
        segments.pop();
    }
    segments
}

/// Every absolute path `source` names. The part from the first `#[cfg(test)]`
/// is the file's `tests` module, where `super` is the file itself.
fn paths_in(file: &Path, source: &str) -> Vec<Vec<String>> {
    let here = module_of(file);
    let (code, tests) = match test_module_start(source) {
        Some(at) => source.split_at(at),
        None => (source, ""),
    };
    let mut in_tests = here.clone();
    in_tests.push("tests".to_string());
    rooted_paths(&code_only(code))
        .iter()
        .map(|path| resolve(path, &here))
        .chain(
            rooted_paths(&code_only(tests))
                .iter()
                .map(|path| resolve(path, &in_tests)),
        )
        .collect()
}

/// The commands: top-level modules of `src/commands/` with an
/// `impl CliCommand for` somewhere in them, with their files.
fn command_modules() -> BTreeMap<String, Vec<PathBuf>> {
    let commands = src_dir().join("commands");
    let mut by_unit: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let files = rust_files(&commands);
    for file in files {
        let unit = unit_of(&file);
        if unit != "mod" {
            by_unit.entry(unit).or_default().push(file);
        }
    }
    by_unit.retain(|_, files| {
        files.iter().any(|file| {
            fs::read_to_string(file)
                .unwrap()
                .contains("impl CliCommand for")
        })
    });
    by_unit
}

/// The top-level module of `src/commands/` a file belongs to.
fn unit_of(file: &Path) -> String {
    let relative = file.strip_prefix(src_dir().join("commands")).unwrap();
    let first = relative
        .components()
        .next()
        .unwrap()
        .as_os_str()
        .to_string_lossy();
    first.trim_end_matches(".rs").to_string()
}

#[test]
fn the_command_list_is_derived_from_the_registry_and_complete() {
    let commands = command_modules();
    let registry = fs::read_to_string(src_dir().join("commands/mod.rs")).unwrap();
    for unit in commands.keys() {
        assert!(
            registry.contains(&format!("Entry::of::<{unit}::")),
            "`commands::{unit}` implements CliCommand but `commands::entries` registers none of its commands"
        );
    }
    assert!(
        commands.len() >= 20,
        "found only {:?}: the CliCommand scan broke",
        commands.keys().collect::<Vec<_>>()
    );
    assert!(
        !commands.contains_key("common"),
        "`commands::common` is the helper layer and holds no CliCommand"
    );
}

/// Every import of another command's module, as `(file, importer, imported)`.
fn command_to_command_imports(
    commands: &BTreeMap<String, Vec<PathBuf>>,
) -> Vec<(String, String, String)> {
    let names: BTreeSet<&str> = commands.keys().map(String::as_str).collect();
    let files = rust_files(&src_dir().join("commands"));
    let mut found = Vec::new();
    for file in files {
        if file == src_dir().join("commands/mod.rs") {
            continue;
        }
        let unit = unit_of(&file);
        let source = fs::read_to_string(&file).unwrap();
        for path in paths_in(&file, &source) {
            if let [root, commands_module, target, ..] = path.as_slice()
                && root == "crate"
                && commands_module == "commands"
                && target != &unit
                && names.contains(target.as_str())
            {
                found.push((
                    file.strip_prefix(src_dir()).unwrap().display().to_string(),
                    unit.clone(),
                    target.clone(),
                ));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn no_command_imports_another_command() {
    let found = command_to_command_imports(&command_modules());
    assert!(
        found.is_empty(),
        "a command module reaches into another command; move what they share into \
         commands/common, exchange or a domain module:\n{}",
        found
            .iter()
            .map(|(file, from, to)| format!("  {file}: {from} -> {to}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The modules the binary crate declares in `main.rs`: the application edge.
fn binary_files() -> BTreeSet<PathBuf> {
    let main = src_dir().join("main.rs");
    let mut files = BTreeSet::from([main.clone()]);
    for line in code_only(&fs::read_to_string(&main).unwrap()).lines() {
        let line = line.trim();
        if let Some(name) = line
            .strip_prefix("mod ")
            .or_else(|| line.strip_prefix("pub mod "))
            .and_then(|rest| rest.strip_suffix(';'))
        {
            let file = src_dir().join(format!("{name}.rs"));
            files.insert(if file.exists() {
                file
            } else {
                src_dir().join(name).join("mod.rs")
            });
        }
    }
    files
}

#[test]
fn nothing_outside_commands_imports_commands() {
    let binary = binary_files();
    let files = rust_files(&src_dir());
    let mut found = Vec::new();
    for file in files {
        let relative = file.strip_prefix(src_dir()).unwrap().display().to_string();
        if relative.starts_with("commands/") || binary.contains(&file) {
            continue;
        }
        let source = fs::read_to_string(&file).unwrap();
        if paths_in(&file, &source)
            .iter()
            .any(|path| path.get(1).is_some_and(|segment| segment == "commands"))
        {
            found.push(relative);
        }
    }
    assert!(
        found.is_empty(),
        "a module outside `commands` imports it; the dependency points the wrong way \
         (move the shared item out of `commands`):\n{}",
        found.join("\n")
    );
}

/// The scan is not vacuous: every command file that spells an import of the
/// shared layer in plain text (`use crate::commands::common`) has it found and
/// resolved by the scan, and many do.
#[test]
fn the_scan_resolves_the_shared_layer_imports_commands_really_make() {
    let mut importers = 0;
    for file in rust_files(&src_dir().join("commands")) {
        let source = fs::read_to_string(&file).unwrap();
        if !source
            .lines()
            .any(|line| line.trim_start().starts_with("use crate::commands::common"))
        {
            continue;
        }
        importers += 1;
        assert!(
            paths_in(&file, &source)
                .iter()
                .any(|path| path.starts_with(&["crate", "commands", "common"].map(String::from))),
            "{} imports commands::common but the scan does not see it",
            file.display()
        );
    }
    assert!(importers >= 10, "found only {importers} importers");
}

/// The leaves the signing, command-spec and asset graph hang on keep pointing down:
/// each `(files, forbidden)` row says the files import none of the modules.
/// What they would need from a forbidden module belongs in a leaf below both
/// (`signable`, `signature`, `nonce`, `cli_command`), so the providers, the
/// specs and the HTTP edge never form a cycle through `signing` or
/// `invocation`.
#[test]
fn the_signing_and_spec_leaves_import_nothing_above_them() {
    let rows: &[(&[&str], &[&str])] = &[
        (
            &["signable.rs", "signature.rs", "nonce.rs"],
            &[
                "signing",
                "bankr",
                "ows",
                "invocation",
                "http_api",
                "exchange",
                "command_spec",
            ],
        ),
        (
            &["command_spec.rs"],
            &["invocation", "bankr", "ows", "signing", "exchange"],
        ),
        (&["network.rs"], &["http_api", "ws"]),
        (
            &[
                "assets/asset_id.rs",
                "assets/dexes.rs",
                "assets/market.rs",
                "assets/metadata.rs",
                "assets/mod.rs",
                "assets/outcome_description.rs",
                "assets/outcome_templates.rs",
                "assets/outcomes.rs",
                "assets/query.rs",
                "assets/resolver.rs",
            ],
            &["output"],
        ),
        (&["http_api.rs"], &["signing", "bankr", "ows"]),
        (
            &["bankr.rs", "ows/mod.rs", "ows/signing.rs", "ows/vault.rs"],
            &["signing"],
        ),
        (
            &[
                "exchange/mod.rs",
                "exchange/actions.rs",
                "exchange/funds.rs",
                "exchange/send_asset.rs",
                "exchange/user_signed.rs",
                "exchange/builder_approval.rs",
            ],
            &["invocation"],
        ),
    ];
    let mut found = Vec::new();
    for (files, forbidden) in rows {
        for relative in *files {
            let file = src_dir().join(relative);
            let source = fs::read_to_string(&file).unwrap();
            for path in paths_in(&file, &source) {
                if let Some(target) = path.get(1)
                    && forbidden.contains(&target.as_str())
                    && module_of(&file).first() != Some(target)
                {
                    found.push(format!("{relative} -> {}", path.join("::")));
                }
            }
        }
    }
    found.sort();
    found.dedup();
    assert!(
        found.is_empty(),
        "a leaf imports a module above it:\n{}",
        found.join("\n")
    );
}

/// Production code reads the wall clock through `clock`, so "now" is one
/// definition and a test can see every place that asks. Test modules are cut
/// at their `#[cfg(test)]`, the way the scan above reads a file.
#[test]
fn only_the_clock_reads_the_wall_clock() {
    let mut offenders = Vec::new();
    for file in rust_files(&src_dir()) {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        if name == "clock.rs" || name == "tests.rs" {
            continue;
        }
        let source = fs::read_to_string(&file).unwrap();
        let code = production_code(&source);
        if code.contains("Utc::now(") || code.contains("SystemTime::now(") {
            offenders.push(file.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "these read the wall clock directly; use clock::now_utc or clock::now_ms:\n  {}",
        offenders.join("\n  ")
    );
}

/// Where the unit tests start: the first `#[cfg(test)]` that gates a module.
/// One that gates a single item is production code.
fn test_module_start(source: &str) -> Option<usize> {
    let marker = "#[cfg(test)]";
    let mut from = 0;
    while let Some(found) = source[from..].find(marker) {
        let start = from + found;
        let after = source[start + marker.len()..].trim_start();
        if after.starts_with("mod ") || after.starts_with("pub mod ") {
            return Some(start);
        }
        from = start + marker.len();
    }
    None
}

/// `source` as production code: comments and literals blanked, and the unit
/// tests cut off.
fn production_code(source: &str) -> String {
    let code = code_only(source);
    match test_module_start(&code) {
        Some(at) => code[..at].to_string(),
        None => code,
    }
}

/// The print macros panic on a closed pipe, and each used to be a policy of its
/// own. `output::sink` is the one writer to stdout and stderr: it decides what
/// a reader that went away means.
#[test]
fn nothing_prints_but_through_the_output_sink() {
    let mut offenders = Vec::new();
    for file in rust_files(&src_dir()) {
        if file.ends_with("output/sink.rs") {
            continue;
        }
        let code = production_code(&fs::read_to_string(&file).unwrap());
        for name in ["println!", "eprintln!", "print!", "eprint!", "dbg!"] {
            for (at, _) in code.match_indices(name) {
                let glued = code[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if !glued {
                    offenders.push(format!("{}: {name}", file.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "print through `output::sink` instead:\n{}",
        offenders.join("\n")
    );
}

/// The scan is not vacuous: it sees a print macro in code and ignores one in a
/// string, a comment or the unit tests.
#[test]
fn the_print_scan_reads_code_and_not_prose_or_tests() {
    let source = "fn f() { eprintln!(\"x\"); }\n// println!(\"x\")\nfn g() { let _ = \"print!\"; }\n#[cfg(test)]\nmod tests { fn t() { println!(); } }";
    let code = production_code(source);
    assert!(code.contains("eprintln!"));
    assert!(!code.contains("\nprintln!") && !code.contains(" println!"));
    assert!(!code.contains("print!(\"") && !code.contains("tests"));
}
