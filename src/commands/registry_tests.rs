//! The registry the commands build, checked against the commands themselves:
//! that `SCHEMA_ORDER` lists every registered command, that each has one
//! contract, and that the commands the error advice names exist.

use std::collections::BTreeSet;

use super::{entries, registry};
use crate::command_registry::SCHEMA_ORDER;
use crate::command_spec::{Mutability, NetworkUse};

#[test]
fn schema_order_lists_every_registered_command_exactly_once() {
    let listed = SCHEMA_ORDER.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(
        listed.len(),
        SCHEMA_ORDER.len(),
        "SCHEMA_ORDER has a duplicate"
    );
    let registered = entries()
        .iter()
        .map(|entry| entry.spec.key())
        .collect::<BTreeSet<_>>();
    let missing = registered
        .iter()
        .filter(|key| !listed.contains(key.as_str()))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "not in SCHEMA_ORDER: {missing:?}. A command is registered twice on purpose: \
         in commands::entries (the order `--help` lists commands) and here (the order \
         `schema` and the contract fixtures list them); both orders are public and pinned."
    );
    let unknown = listed
        .iter()
        .filter(|key| !registered.contains(**key))
        .collect::<Vec<_>>();
    assert!(
        unknown.is_empty(),
        "SCHEMA_ORDER names no registered command: {unknown:?}. Remove it here, or \
         register the command in commands::entries."
    );
}

#[test]
fn every_registered_command_has_exactly_one_contract() {
    let contracts = registry().commands();
    let paths = contracts
        .iter()
        .map(|command| command.command_path.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(contracts.len(), entries().len());
    assert_eq!(paths.len(), contracts.len(), "two contracts share a path");
}

/// The command words after each `hyperliquid ` in `text`, up to the first
/// flag, placeholder or closing punctuation.
fn advised_commands(text: &str) -> Vec<Vec<String>> {
    text.match_indices("hyperliquid ")
        .map(|(at, found)| {
            let mut words = Vec::new();
            for token in text[at + found.len()..].split_whitespace() {
                let word = token.trim_end_matches(['`', ',', ';', '.', ')', ':']);
                let plain = !word.is_empty()
                    && !word.starts_with('-')
                    && word
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
                if !plain {
                    break;
                }
                words.push(word.to_string());
                if word.len() != token.len() {
                    break;
                }
            }
            words
        })
        .filter(|words| !words.is_empty())
        .collect()
}

#[test]
fn every_command_the_error_advice_names_is_registered() {
    let advice = crate::errors::command_advice();
    let advised = advice
        .iter()
        .flat_map(|text| advised_commands(text))
        .collect::<Vec<_>>();
    assert!(advised.len() >= 10, "found only {advised:?}");
    for words in advised {
        assert!(
            entries().iter().any(|entry| {
                words.len() >= entry.spec.path.len()
                    && entry.spec.path.iter().zip(&words).all(|(a, b)| a == b)
            }),
            "error advice names `hyperliquid {}`, which is not a command",
            words.join(" ")
        );
    }
}

#[test]
fn advice_extraction_stops_at_flags_placeholders_and_punctuation() {
    assert_eq!(
        advised_commands("see `hyperliquid account rate-limit`; or `hyperliquid --help`"),
        [["account", "rate-limit"]]
    );
    assert_eq!(
        advised_commands("run hyperliquid orders create --dry-run, then hyperliquid book."),
        [vec!["orders", "create"], vec!["book"]]
    );
    assert_eq!(
        advised_commands("use `hyperliquid outcomes get #1` now"),
        [["outcomes", "get"]]
    );
}

#[test]
fn a_command_that_needs_no_network_is_not_a_live_or_signed_one() {
    let mut probed_locals = Vec::new();
    let mut networkless_live = Vec::new();
    for contract in registry().commands() {
        let spec = contract.spec;
        if spec.mutability() == Mutability::LocalOnly && spec.network == NetworkUse::Required {
            probed_locals.push(contract.command_key());
        }
        if spec.network == NetworkUse::None
            && (spec.auth_required || spec.signs.is_some() || spec.stream.is_some())
        {
            networkless_live.push(contract.command_key());
        }
    }
    assert!(
        probed_locals.is_empty(),
        "these change local state only, so they must not probe the API override: {probed_locals:?}"
    );
    assert!(
        networkless_live.is_empty(),
        "these name no network but sign or stream: {networkless_live:?}"
    );
}
