//! `account abstraction set` and `account evm-big-blocks`: the account actions
//! that change how an account behaves, apart from the `account` reads.

use alloy::sol;
use clap::{Args, ValueEnum};
use hypersdk::Address;
use hypersdk::hypercore::types::Action;
use hypersdk::hypercore::{AbstractionMode, Chain, UserRole};
use serde::Serialize;

use crate::account_mode::AccountMode;
use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, InputKind, Risk, YES_FOR_LIVE_EXECUTION, arg};
use crate::commands::common::rows::{One, Row};
use crate::commands::common::support::{SUBMITTED, action_json, print_dry_run};
use crate::consent::{self, ConsentBlock};
use crate::errors::CliError;
use crate::exchange::actions;
use crate::exchange::user_signed;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::network::ChainExt;
use crate::signable::{Signs, UserSigned};
use crate::signing::{AgentForm, SelectedSigner};
use crate::wire::InfoRequest;

sol! {
    #[derive(Serialize)]
    struct UserSetAbstraction {
        string hyperliquidChain;
        address user;
        string abstraction;
        uint64 nonce;
    }
}

impl UserSigned for UserSetAbstraction {
    const ACTION_TYPE: &'static str = "userSetAbstraction";
}

#[derive(Args, Debug, Clone)]
pub struct SetAbstractionArgs {
    /// Abstraction mode to set
    #[arg(long, value_enum)]
    pub mode: AbstractionModeArg,

    /// Subaccount of the signer to set the mode for: subaccount address, stored account alias, or stored account id. Defaults to the signer's own account
    #[arg(long)]
    pub user: Option<String>,

    /// Confirm live execution without prompting
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// The modes `account abstraction set` can set. Both the protocol value and the
/// CLI spelling come from the SDK's mode through [`AccountMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AbstractionModeArg {
    Disabled,
    UnifiedAccount,
    PortfolioMargin,
}

impl AbstractionModeArg {
    fn sdk_mode(self) -> AbstractionMode {
        match self {
            Self::Disabled => AbstractionMode::Standard,
            Self::UnifiedAccount => AbstractionMode::UnifiedAccount,
            Self::PortfolioMargin => AbstractionMode::PortfolioMargin,
        }
    }

    fn protocol_value(self) -> &'static str {
        self.sdk_mode().api_str()
    }

    fn cli_value(self) -> &'static str {
        AccountMode::from(self.sdk_mode()).as_str()
    }
}

#[derive(Debug, Clone, Serialize)]
struct AbstractionSetRow {
    status: String,
    action: String,
    signer: String,
    user: String,
    mode: String,
    protocol_mode: String,
    network: String,
}

impl Row for AbstractionSetRow {
    const HEADERS: &'static [&'static str] =
        &["Status", "Action", "Signer", "User", "Mode", "Network"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.signer.clone(),
            self.user.clone(),
            self.mode.clone(),
            self.network.clone(),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbstractionPlan {
    user: Address,
    /// An API wallet cannot sign `userSetAbstraction`; it signs
    /// `agentSetAbstraction`, which only ever applies to its master account.
    agent_signed: bool,
}

fn abstraction_plan(
    form: Option<AgentForm>,
    signer: Address,
    requested: Option<Address>,
) -> Result<AbstractionPlan, CliError> {
    if let Some(AgentForm { master }) = form {
        if let Some(requested) = requested
            && requested != master
        {
            return Err(CliError::Unsupported(format!(
                "an API wallet can only set the abstraction of its own master account ({master}), not {requested}; use the master wallet for a subaccount"
            )));
        }
        return Ok(AbstractionPlan {
            user: master,
            agent_signed: true,
        });
    }
    Ok(AbstractionPlan {
        user: requested.unwrap_or(signer),
        agent_signed: false,
    })
}

/// `userSetAbstraction` may name the signer or one of its subaccounts.
async fn ensure_signer_owns(
    api: &ApiClient,
    signer: Address,
    user: Address,
) -> Result<(), CliError> {
    if user == signer {
        return Ok(());
    }
    match api
        .info::<UserRole>(
            &InfoRequest::user("userRole", user),
            "loading the role of the target account",
        )
        .await?
    {
        UserRole::SubAccount { master } if master == signer => Ok(()),
        _ => Err(CliError::Unsupported(format!(
            "{user} is not a subaccount of the signer {signer}; --user must name the signer's own account or one of its subaccounts"
        ))),
    }
}

fn abstraction_set_dry_run_value(
    chain: Chain,
    plan: Option<AbstractionPlan>,
    mode: AbstractionModeArg,
) -> Result<serde_json::Value, CliError> {
    let user = plan.map(|plan| plan.user.to_string());
    let agent_signed = plan.is_some_and(|plan| plan.agent_signed);
    let action = if agent_signed {
        action_json(&Action::AgentSetAbstraction {
            abstraction: mode.sdk_mode(),
        })?
    } else {
        let mut action = user_signed::preview(
            &UserSetAbstraction {
                hyperliquidChain: chain.label().to_string(),
                user: plan.map_or(Address::ZERO, |plan| plan.user),
                abstraction: mode.protocol_value().to_string(),
                nonce: 0,
            },
            chain,
        );
        action["user"] = user.clone().into();
        action
    };
    let mut preview = serde_json::json!({
        "mode": mode.cli_value(),
        "protocol_mode": mode.protocol_value(),
        "network": chain.label(),
        "user": user,
        "action": action,
    });
    if agent_signed {
        preview["agent_signed"] = true.into();
    }
    Ok(preview)
}

async fn set_abstraction(
    inv: &Invocation,
    signer: &SelectedSigner,
    plan: AbstractionPlan,
    args: &SetAbstractionArgs,
) -> Result<(), anyhow::Error> {
    let user = plan.user;
    let remote = inv.remote()?;
    if !plan.agent_signed {
        ensure_signer_owns(remote.api(), signer.address(), user).await?;
    }
    let question = format!(
        "Set account abstraction for {user} to {}?",
        args.mode.cli_value()
    );
    consent::approve(
        inv,
        args.yes,
        &ConsentBlock::new(),
        &question,
        "account abstraction update aborted",
    )?;
    let protocol_mode = args.mode.protocol_value();
    if plan.agent_signed {
        actions::send_default_l1_action(
            remote.api(),
            signer,
            Action::AgentSetAbstraction {
                abstraction: args.mode.sdk_mode(),
            },
        )
        .await?;
    } else {
        user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| {
            UserSetAbstraction {
                hyperliquidChain: hyperliquid_chain,
                user,
                abstraction: protocol_mode.to_string(),
                nonce,
            }
        })
        .await?;
    }

    inv.print(&One::new(AbstractionSetRow {
        status: SUBMITTED.to_string(),
        action: "set-abstraction".to_string(),
        signer: signer.address().to_string(),
        user: user.to_string(),
        mode: args.mode.cli_value().to_string(),
        protocol_mode: protocol_mode.to_string(),
        network: remote.chain().label().to_string(),
    }));
    Ok(())
}

pub struct AccountAbstractionSet;

impl CliCommand for AccountAbstractionSet {
    type Args = SetAbstractionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["account", "abstraction", "set"],
        "account",
        "Set abstraction mode for the selected signer or one of its subaccounts",
        "Set account abstraction for the signer or one of its subaccounts (userSetAbstraction); an API wallet sets its master account's mode with agentSetAbstraction. `disabled` is Standard: separate perp and spot balances. Cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode; unified-account and portfolio-margin accounts are limited to 50k actions per day. Use --dry-run to preview; confirm or pass -y for live submit.",
    )
    .long_about("Set account abstraction for the signer or one of its subaccounts (userSetAbstraction).\n\
\n\
`disabled` is Standard: separate perp and spot balances. `unified-account` keeps one balance per asset for spot and every perp dex; `portfolio-margin` margins eligible assets together.\n\
\n\
Cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode. Unified-account and portfolio-margin accounts are limited to 50k actions per day.\n\
\n\
--user names a subaccount of the signer. An API wallet signs agentSetAbstraction instead, which changes its master account's mode only; --user may not name another account.\n\
\n\
Use --dry-run to preview; confirm or pass -y for live submit.")
    .signs(Signs::user_signed::<UserSetAbstraction>())
    .risk(Risk::AccountState)
    .prompt()
    .args(&[
        arg("mode").describe("Target abstraction mode."),
        arg("user").kind(InputKind::ActingAccountSelector).describe("Subaccount of the signer to set the mode for: subaccount address, wallet name, or wallet id. Defaults to the signer's own account.").resource_id("subaccount selector"),
        YES_FOR_LIVE_EXECUTION,
    ]);

    async fn run(inv: &Invocation, args: &SetAbstractionArgs) -> Result<(), anyhow::Error> {
        let requested = args
            .user
            .as_deref()
            .map(|selector| inv.resolve_account(selector))
            .transpose()?;
        if inv.dry_run() {
            let plan = match inv.dry_run_signer()? {
                Some(signer) => Some(abstraction_plan(
                    signer.agent_form(),
                    signer.address(),
                    requested,
                )?),
                None => requested.map(|user| AbstractionPlan {
                    user,
                    agent_signed: false,
                }),
            };
            return print_dry_run(
                inv,
                "set_abstraction",
                abstraction_set_dry_run_value(inv.remote()?.chain(), plan, args.mode)?,
            );
        }
        let signer = inv.resolve_signer()?;
        let plan = abstraction_plan(signer.agent_form(), signer.address(), requested)?;
        set_abstraction(inv, &signer, plan, args).await
    }
}

/// Arguments for `account evm-big-blocks`.
#[derive(Args, Debug, Clone)]
pub struct EvmBigBlocksArgs {
    /// Send the account's HyperEVM transactions to big blocks
    #[arg(long, conflicts_with = "disable", required_unless_present = "disable")]
    pub enable: bool,

    /// Send the account's HyperEVM transactions back to small blocks
    #[arg(long)]
    pub disable: bool,
}

#[derive(Debug, Clone, Serialize)]
struct EvmBigBlocksRow {
    status: String,
    action: String,
    signer: String,
    acting_as: String,
    using_big_blocks: bool,
    network: String,
}

impl Row for EvmBigBlocksRow {
    const HEADERS: &'static [&'static str] = &[
        "Status",
        "Action",
        "Signer",
        "Acting As",
        "Big Blocks",
        "Network",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.signer.clone(),
            self.acting_as.clone(),
            self.using_big_blocks.to_string(),
            self.network.clone(),
        ]
    }
}

pub struct AccountEvmBigBlocks;

impl CliCommand for AccountEvmBigBlocks {
    type Args = EvmBigBlocksArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["account", "evm-big-blocks"],
        "account",
        "Direct the account's HyperEVM transactions to big or small blocks",
        "Set the HyperCore user flag that routes the account's HyperEVM transactions to big blocks (--enable) or small blocks (--disable) (evmUserModify). Big blocks are for large contract deployments; the flag stays set until it is unset again. The account must already exist on HyperCore.",
    )
    .risk(Risk::AccountState)
    .one_of_required(&[&["enable"], &["disable"]])
    .args(&[
        arg("enable")
            .kind(InputKind::Boolean)
            .describe("Route HyperEVM transactions to big blocks."),
        arg("disable")
            .kind(InputKind::Boolean)
            .describe("Route HyperEVM transactions back to small blocks."),
    ]);

    async fn run(inv: &Invocation, args: &EvmBigBlocksArgs) -> Result<(), anyhow::Error> {
        let using_big_blocks = args.enable;
        let action = Action::EvmUserModify { using_big_blocks };
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "set_evm_big_blocks",
                serde_json::json!({
                    "using_big_blocks": using_big_blocks,
                    "network": inv.remote()?.chain().label(),
                    "action": action_json(&action)?,
                }),
            );
        }
        let signer = inv.resolve_signer()?;
        let remote = inv.remote()?;
        actions::send_default_l1_action(remote.api(), &signer, action).await?;
        inv.print(&One::new(EvmBigBlocksRow {
            status: SUBMITTED.to_string(),
            action: "evm-big-blocks".to_string(),
            signer: signer.address().to_string(),
            acting_as: signer.query_address().to_string(),
            using_big_blocks,
            network: remote.chain().label().to_string(),
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn an_api_wallet_sets_its_masters_mode_and_no_other_accounts() {
        let master = Address::repeat_byte(1);
        let agent = Address::repeat_byte(9);
        let form = Some(AgentForm { master });

        assert_eq!(
            abstraction_plan(form, agent, None).unwrap(),
            AbstractionPlan {
                user: master,
                agent_signed: true
            }
        );
        assert!(
            abstraction_plan(form, agent, Some(master))
                .unwrap()
                .agent_signed
        );
        let err = abstraction_plan(form, agent, Some(Address::repeat_byte(2))).unwrap_err();
        assert_eq!(err.exit_code(), 13);
    }

    #[test]
    fn any_other_signer_sets_its_own_or_a_named_account_with_the_user_signed_action() {
        let own = Address::repeat_byte(9);
        let subaccount = Address::repeat_byte(2);

        assert_eq!(
            abstraction_plan(None, own, None).unwrap(),
            AbstractionPlan {
                user: own,
                agent_signed: false
            }
        );
        assert_eq!(
            abstraction_plan(None, own, Some(subaccount)).unwrap(),
            AbstractionPlan {
                user: subaccount,
                agent_signed: false
            }
        );
    }

    #[test]
    fn a_settable_mode_is_spelled_the_same_on_the_command_line_and_in_reports() {
        for mode in AbstractionModeArg::value_variants() {
            let flag = mode.to_possible_value().unwrap();
            assert_eq!(flag.get_name(), mode.cli_value());
            let reported = AccountMode::parse(mode.protocol_value());
            assert_ne!(reported, AccountMode::Unknown, "{mode:?}");
            assert_eq!(reported.as_str(), mode.cli_value());
        }
    }
}
