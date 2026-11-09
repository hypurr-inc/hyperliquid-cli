pub mod abstraction;
pub mod account;
pub(crate) mod actions;
pub mod api_wallet;
pub mod asset;
pub mod borrowlend;
pub mod builder;
pub(crate) mod confirm;
pub mod feedback;
pub(crate) mod issue_report;
pub mod mcp;
pub mod meta;
pub mod orderbook;
pub mod orders;
pub mod outcomes;
pub mod perps;
pub mod positions;
pub mod prio;
pub mod referral;
pub mod report_incident;
pub mod rows;
pub mod schema;
pub mod selector;
pub mod setup;
pub mod spot;
pub(crate) mod spot_balances;
pub mod staking;
pub mod status;
pub mod subaccounts;
pub mod subscribe;
pub(crate) mod support;
pub mod transfers;
pub mod update;
mod user_signed;
pub mod vaults;
pub mod wallet;
pub mod watch_risk;

use std::sync::LazyLock;

use crate::command_spec::{Entry, GroupSpec};

/// Every command, in help order. Registering a command is one line here.
pub fn entries() -> &'static [Entry] {
    static ENTRIES: LazyLock<Vec<Entry>> = LazyLock::new(|| {
        vec![
            Entry::of::<perps::PerpsList>(),
            Entry::of::<perps::PerpsGet>(),
            Entry::of::<spot::SpotList>(),
            Entry::of::<spot::SpotGet>(),
            Entry::of::<asset::AssetDecode>(),
            Entry::of::<asset::AssetSearch>(),
            Entry::of::<outcomes::OutcomesList>(),
            Entry::of::<outcomes::OutcomesGet>(),
            Entry::of::<orderbook::Book>(),
            Entry::of::<orderbook::Mids>(),
            Entry::of::<orderbook::Candles>(),
            Entry::of::<orderbook::Spread>(),
            Entry::of::<orderbook::Funding>(),
            Entry::of::<asset::Search>(),
            Entry::of::<status::Status>(),
            Entry::of::<meta::Meta>(),
            Entry::of::<orders::cli::OrdersOpen>(),
            Entry::of::<orders::cli::OrdersStatus>(),
            Entry::of::<orders::cli::OrdersHistory>(),
            Entry::of::<orders::cli::OrdersCreate>(),
            Entry::of::<orders::cli::OrdersScale>(),
            Entry::of::<orders::cli::OrdersBatchCreate>(),
            Entry::of::<orders::cli::OrdersTpsl>(),
            Entry::of::<orders::cli::OrdersTrailingStop>(),
            Entry::of::<orders::cli::OrdersCancel>(),
            Entry::of::<orders::cli::OrdersCancelAll>(),
            Entry::of::<orders::cli::OrdersModify>(),
            Entry::of::<orders::cli::OrdersBatchModify>(),
            Entry::of::<orders::cli::OrdersTwapCreate>(),
            Entry::of::<orders::cli::OrdersTwapCancel>(),
            Entry::of::<orders::cli::OrdersScheduleCancel>(),
            Entry::of::<orders::cli::OrdersChase>(),
            Entry::of::<orders::cli::OrdersBracket>(),
            Entry::of::<orders::cli::Buy>(),
            Entry::of::<orders::cli::Sell>(),
            Entry::of::<orders::cli::OutcomeBuy>(),
            Entry::of::<orders::cli::OutcomeSell>(),
            Entry::of::<positions::PositionsList>(),
            Entry::of::<positions::PositionsUpdateLeverage>(),
            Entry::of::<positions::PositionsUpdateMargin>(),
            Entry::of::<account::AccountFills>(),
            Entry::of::<account::AccountFees>(),
            Entry::of::<account::AccountRateLimit>(),
            Entry::of::<account::AccountOrders>(),
            Entry::of::<account::AccountPortfolio>(),
            Entry::of::<account::AccountSubaccounts>(),
            Entry::of::<account::AccountPortfolioHistory>(),
            Entry::of::<account::AccountLedger>(),
            Entry::of::<account::AccountFunding>(),
            Entry::of::<account::AccountTwapHistory>(),
            Entry::of::<account::AccountTwapFills>(),
            Entry::of::<account::AccountAbstraction>(),
            Entry::of::<account::AccountAbstractionSet>(),
            Entry::of::<wallet::AccountAdd>(),
            Entry::of::<wallet::AccountLs>(),
            Entry::of::<wallet::AccountSetDefault>(),
            Entry::of::<wallet::AccountRemove>(),
            Entry::of::<wallet::WalletCreate>(),
            Entry::of::<wallet::WalletImport>(),
            Entry::of::<wallet::WalletImportMnemonic>(),
            Entry::of::<wallet::WalletShow>(),
            Entry::of::<wallet::WalletAddress>(),
            Entry::of::<wallet::WalletList>(),
            Entry::of::<wallet::WalletRename>(),
            Entry::of::<wallet::WalletDelete>(),
            Entry::of::<wallet::WalletExport>(),
            Entry::of::<wallet::WalletReset>(),
            Entry::of::<wallet::WalletLogin>(),
            Entry::of::<wallet::WalletLogout>(),
            Entry::of::<api_wallet::ApiWalletCreate>(),
            Entry::of::<api_wallet::ApiWalletApprove>(),
            Entry::of::<api_wallet::ApiWalletList>(),
            Entry::of::<api_wallet::ApiWalletRevoke>(),
            Entry::of::<subaccounts::SubaccountList>(),
            Entry::of::<subaccounts::SubaccountCreate>(),
            Entry::of::<subaccounts::SubaccountTransfer>(),
            Entry::of::<subaccounts::SubaccountSpotTransfer>(),
            Entry::of::<builder::BuilderMaxFee>(),
            Entry::of::<builder::BuilderApproved>(),
            Entry::of::<builder::BuilderApprove>(),
            Entry::of::<referral::ReferralSet>(),
            Entry::of::<referral::ReferralRegister>(),
            Entry::of::<referral::ReferralStatus>(),
            Entry::of::<staking::StakingSummary>(),
            Entry::of::<staking::StakingValidators>(),
            Entry::of::<staking::StakingRewards>(),
            Entry::of::<staking::StakingHistory>(),
            Entry::of::<staking::StakingDelegate>(),
            Entry::of::<staking::StakingUndelegate>(),
            Entry::of::<staking::StakingDeposit>(),
            Entry::of::<staking::StakingWithdraw>(),
            Entry::of::<staking::StakingClaimRewards>(),
            Entry::of::<staking::StakingLinkInitiate>(),
            Entry::of::<staking::StakingLinkFinalize>(),
            Entry::of::<vaults::VaultList>(),
            Entry::of::<vaults::VaultSearch>(),
            Entry::of::<vaults::VaultGet>(),
            Entry::of::<vaults::VaultPositions>(),
            Entry::of::<vaults::VaultDeposit>(),
            Entry::of::<vaults::VaultWithdraw>(),
            Entry::of::<borrowlend::BorrowlendRates>(),
            Entry::of::<borrowlend::BorrowlendGet>(),
            Entry::of::<borrowlend::BorrowlendUser>(),
            Entry::of::<borrowlend::BorrowlendSupply>(),
            Entry::of::<borrowlend::BorrowlendWithdraw>(),
            Entry::of::<prio::PrioStatus>(),
            Entry::of::<prio::PrioBid>(),
            Entry::of::<transfers::TransferSpotToPerp>(),
            Entry::of::<transfers::TransferPerpToSpot>(),
            Entry::of::<transfers::TransferSend>(),
            Entry::of::<transfers::TransferSpotSend>(),
            Entry::of::<transfers::TransferSendAsset>(),
            Entry::of::<transfers::TransferWithdraw>(),
            Entry::of::<subscribe::SubscribeTrades>(),
            Entry::of::<subscribe::SubscribeOrderbook>(),
            Entry::of::<subscribe::SubscribeCandles>(),
            Entry::of::<subscribe::SubscribeAllMids>(),
            Entry::of::<subscribe::SubscribeOrderUpdates>(),
            Entry::of::<subscribe::SubscribeFills>(),
            Entry::of::<watch_risk::WatchRisk>(),
            Entry::of::<schema::Schema>(),
            Entry::of::<setup::Setup>(),
            Entry::of::<feedback::Feedback>(),
            Entry::of::<update::Update>(),
            Entry::of::<mcp::Mcp>(),
            Entry::of::<report_incident::ReportIncident>(),
        ]
    });
    &ENTRIES
}

/// The clap tree of every command, without the global options. Built once;
/// consumers clone it to decorate or `build()` their copy.
pub fn command_tree() -> &'static clap::Command {
    static TREE: LazyLock<clap::Command> =
        LazyLock::new(|| crate::command_spec::command_tree(entries(), groups()));
    &TREE
}

/// The command groups: names with children and no behavior of their own.
pub fn groups() -> &'static [GroupSpec] {
    static GROUPS: &[GroupSpec] = &[
        perps::GROUP,
        spot::GROUP,
        asset::GROUP,
        outcomes::GROUP,
        builder::GROUP,
        account::GROUP,
        api_wallet::GROUP,
        subaccounts::GROUP,
        orders::cli::GROUP,
        watch_risk::GROUP,
        positions::GROUP,
        transfers::GROUP,
        wallet::GROUP,
        staking::GROUP,
        staking::LINK_GROUP,
        vaults::GROUP,
        borrowlend::GROUP,
        prio::GROUP,
        referral::GROUP,
        subscribe::GROUP,
    ];
    GROUPS
}
