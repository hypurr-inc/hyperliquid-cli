use clap::ValueEnum;
use serde::Serialize;

/// Which way money moves between the signing account and a vault or
/// subaccount: `subaccount transfer --direction` and the two vault commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDirection {
    Deposit,
    Withdraw,
}

impl TransferDirection {
    pub fn is_deposit(self) -> bool {
        matches!(self, Self::Deposit)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
            Self::Withdraw => "withdraw",
        }
    }
}

impl std::fmt::Display for TransferDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
