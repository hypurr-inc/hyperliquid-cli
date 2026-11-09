//! Commands this CLI suggests to the user for the orders and positions it
//! places, built in one place so renaming a flag or a command cannot strand
//! advice that other code wrote out by hand. The wallet commands are in
//! `wallet`. This module imports nothing from the crate, so every layer,
//! `errors` included, can name a suggested command.

use std::fmt;

pub mod wallet;

pub const CHECK_LIVE_COMMAND: &str = "hyperliquid orders open";

/// The command that cancels an order, which takes the order id or `--cloid`.
pub const CANCEL_ORDER_COMMAND: &str = "hyperliquid orders cancel";

pub const CANCEL_TWAP_COMMAND: &str = "hyperliquid orders twap-cancel";

pub const CREATE_TWAP_COMMAND: &str = "hyperliquid orders twap-create";

pub const TRAILING_STOP_COMMAND: &str = "hyperliquid orders trailing-stop";

pub const ACCOUNT_FILLS_COMMAND: &str = "hyperliquid account fills";

/// Per-account request budget, which the server reports.
pub const ACCOUNT_RATE_LIMIT_COMMAND: &str = "hyperliquid account rate-limit";

/// The ` --on-behalf-of <selector>` a suggested command needs to act on the
/// same account as this run; empty when the run acts for its own account.
#[must_use]
pub fn on_behalf_of_flag(on_behalf_of: Option<&str>) -> String {
    on_behalf_of
        .map(|selector| format!(" --on-behalf-of {selector}"))
        .unwrap_or_default()
}

/// The `positions update-leverage` command that puts `coin` into isolated margin when `isolated`, else cross, at
/// `leverage` for the account a run acts for. `<N>` is what an order passes
/// when the leverage is the user's to choose.
#[must_use]
pub fn update_leverage_command(
    coin: &str,
    leverage: impl fmt::Display,
    isolated: bool,
    on_behalf_of: Option<&str>,
) -> String {
    let isolated = if isolated { " --isolated" } else { "" };
    format!(
        "positions update-leverage --coin {coin} --leverage {leverage}{isolated}{}",
        on_behalf_of_flag(on_behalf_of)
    )
}

/// The command that cancels the order `target` names (an order id, or
/// `--cloid <id>`).
#[must_use]
pub fn cancel_order_command(target: &str, on_behalf_of: Option<&str>) -> String {
    format!(
        "{CANCEL_ORDER_COMMAND} {target}{}",
        on_behalf_of_flag(on_behalf_of)
    )
}

#[must_use]
pub fn protect_position_command(
    coin: &str,
    take_profit: impl fmt::Display,
    stop_loss: impl fmt::Display,
    on_behalf_of: Option<&str>,
) -> String {
    format!(
        "hyperliquid orders tpsl --coin {coin} --take-profit {take_profit} --stop-loss {stop_loss}{}",
        on_behalf_of_flag(on_behalf_of)
    )
}

/// The command that approves `builder` at `max_fee_rate` with the main wallet.
#[must_use]
pub fn builder_approve_command(
    builder: impl fmt::Display,
    max_fee_rate: impl fmt::Display,
) -> String {
    format!("hyperliquid builder approve --builder {builder} --max-fee-rate {max_fee_rate}")
}

/// The command that burns `nonce` on the network a run used.
#[must_use]
pub fn invalidate_nonce_command(testnet: bool, nonce: impl fmt::Display) -> String {
    let network = if testnet { " --testnet" } else { "" };
    format!("hyperliquid{network} account invalidate-nonce --nonce {nonce}")
}

/// The command that shows the encoded HIP-4 asset id of an outcome.
#[must_use]
pub fn outcome_get_command(asset: impl fmt::Display) -> String {
    format!("hyperliquid outcomes get {asset}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commands_that_carry_a_value_spell_it_once() {
        assert_eq!(
            builder_approve_command("0xB", "0.05%"),
            "hyperliquid builder approve --builder 0xB --max-fee-rate 0.05%"
        );
        assert_eq!(
            invalidate_nonce_command(false, 7),
            "hyperliquid account invalidate-nonce --nonce 7"
        );
        assert_eq!(
            invalidate_nonce_command(true, 7),
            "hyperliquid --testnet account invalidate-nonce --nonce 7"
        );
        assert_eq!(outcome_get_command("#1"), "hyperliquid outcomes get #1");
    }

    #[test]
    fn the_account_flag_follows_the_selector() {
        assert_eq!(on_behalf_of_flag(None), "");
        assert_eq!(on_behalf_of_flag(Some("vault")), " --on-behalf-of vault");
        assert_eq!(
            cancel_order_command("--cloid 0x1", Some("vault")),
            "hyperliquid orders cancel --cloid 0x1 --on-behalf-of vault"
        );
    }

    #[test]
    fn update_leverage_names_the_mode_and_the_account() {
        assert_eq!(
            update_leverage_command("BTC", "<N>", false, None),
            "positions update-leverage --coin BTC --leverage <N>"
        );
        assert_eq!(
            update_leverage_command("BTC", 5, true, Some("v")),
            "positions update-leverage --coin BTC --leverage 5 --isolated --on-behalf-of v"
        );
    }
}
