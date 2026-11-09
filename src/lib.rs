//! Hyperliquid CLI library.
//!
//! Exposes internal modules for integration testing.

/// The id of the flag that answers a confirmation prompt. A macro so static
/// text can `concat!` it; use [`command_spec::YES_ARG`] elsewhere.
#[macro_export]
macro_rules! yes_arg {
    () => {
        "yes"
    };
}

/// The flag as typed, for static text built with `concat!`; use
/// [`command_spec::YES_FLAG`] elsewhere.
#[macro_export]
macro_rules! yes_flag {
    () => {
        concat!("--", $crate::yes_arg!())
    };
}

pub mod account_mode;
pub mod assets;
pub mod auth;
pub mod bankr;
pub mod bankr_credentials;
pub mod builder_fee;
pub mod clock;
pub mod command_registry;
pub mod command_spec;
pub mod commands;
pub mod config;
pub mod consent;
pub mod dry_run;
pub mod env;
pub mod errors;
pub mod exchange;
pub mod exchange_reply;
pub mod hints;
pub mod http_api;
pub mod input_hardening;
pub mod invocation;
pub mod legacy_accounts;
pub mod margin;
pub mod mcp;
pub mod mode;
pub mod network;
pub mod output;
pub mod ows;
pub mod paths;
pub mod private_fs;
pub mod prompt;
pub mod quantity;
pub mod referral_code;
pub mod report;
pub mod resolvers;
pub mod response_sanitization;
pub mod settings;
pub mod signing;
pub mod spot_balances;
pub mod stream;
pub mod stream_bounds;
pub mod terminal;
pub mod update;
pub mod wallet_store;
pub mod watch;
pub mod wire;
pub mod wire_decimal;
pub mod ws;
