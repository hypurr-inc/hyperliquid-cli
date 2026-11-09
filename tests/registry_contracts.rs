mod contract_support;

use hyperliquid_cli::command_registry::CommandRegistry;
use hyperliquid_cli::command_spec::{
    ConfirmationPolicy, DryRunPolicy, InputKind, Lifecycle, OutputContract, RawPayloadPolicy, Risk,
};
use serde_json::json;

use contract_support::{command_path, schema_all};

#[test]
fn the_schema_is_the_registry_serialized() {
    let registry = CommandRegistry::get();
    let schemas = schema_all();
    assert_eq!(schemas.len(), registry.commands().len());

    for (command, schema) in registry.commands().iter().zip(&schemas) {
        let key = command.command_key();
        assert_eq!(command_path(schema), key, "schema order is registry order");

        let entry = serde_json::to_value(command).unwrap();
        let entry = entry.as_object().unwrap();
        for (field, value) in entry {
            assert_eq!(&schema[field], value, "{key}: field {field}");
        }
        assert_eq!(
            schema.as_object().unwrap().len(),
            entry.len() + 1,
            "{key}: the schema carries only the contract's fields plus json_schema"
        );

        let policy = serde_json::to_value(&command.policy).unwrap();
        assert_eq!(
            schema["json_schema"]["x-hyperliquid"], policy,
            "{key}: x-hyperliquid is the contract's policy"
        );
        for field in policy.as_object().unwrap().keys() {
            assert_eq!(
                schema["json_schema"]["x-hyperliquid"][field], schema[field],
                "{key}: policy field {field} is reported once"
            );
        }

        let one_of = command
            .spec
            .one_of_required
            .iter()
            .map(|required| json!({ "required": required }))
            .collect::<Vec<_>>();
        assert_eq!(
            schema["json_schema"]["oneOf"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            one_of,
            "{key}: oneOf"
        );

        let properties = schema["json_schema"]["properties"].as_object().unwrap();
        assert_eq!(properties.len(), command.inputs.len(), "{key}: properties");
        for input in &command.inputs {
            assert!(properties.contains_key(&input.id), "{key}: {}", input.id);
        }
    }
}

#[test]
fn registry_explicitly_classifies_critical_command_classes() {
    let registry = CommandRegistry::get();

    let orders_create = registry.find_path(&["orders", "create"]).unwrap();
    assert!(orders_create.dangerous);
    assert_eq!(orders_create.risk, Risk::FundsMovement);
    assert_eq!(orders_create.dry_run, DryRunPolicy::Optional);
    assert_eq!(orders_create.raw_payload, RawPayloadPolicy::DryRunOnly);
    assert_eq!(orders_create.confirmation, ConfirmationPolicy::Prompt);

    let prio_status = registry.find_path(&["prio", "status"]).unwrap();
    assert_eq!(prio_status.risk, Risk::None);
    assert_eq!(prio_status.dry_run, DryRunPolicy::NotSupported);
    assert_eq!(prio_status.raw_payload, RawPayloadPolicy::Unsupported);
    assert_eq!(prio_status.confirmation, ConfirmationPolicy::None);

    let account_abstraction_set = registry
        .find_path(&["account", "abstraction", "set"])
        .unwrap();
    assert_eq!(account_abstraction_set.risk, Risk::AccountState);
    assert_eq!(account_abstraction_set.dry_run, DryRunPolicy::Optional);

    let subscribe = registry.find_path(&["subscribe", "all-mids"]).unwrap();
    assert_eq!(subscribe.lifecycle, Lifecycle::Streaming);
    assert_eq!(
        subscribe.output_contract,
        OutputContract::BoundedNdjsonStream
    );
    assert!(!subscribe.transport.is_empty());

    for path in [
        &["setup"][..],
        &["wallet", "create"][..],
        &["wallet", "import"][..],
        &["wallet", "reset"][..],
        &["account", "add"][..],
        &["account", "set-default"][..],
        &["account", "remove"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.lifecycle, Lifecycle::InteractiveLocal, "{path:?}");
    }

    for path in [
        &["setup"][..],
        &["wallet", "create"][..],
        &["wallet", "import"][..],
        &["wallet", "show"][..],
        &["wallet", "address"][..],
        &["account", "add"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.risk, Risk::LocalSecret, "{path:?}");
    }

    for path in [
        &["account", "ls"][..],
        &["account", "set-default"][..],
        &["account", "remove"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.risk, Risk::LocalState, "{path:?}");
    }

    let staking_link_initiate = registry
        .find_path(&["staking", "link", "initiate"])
        .unwrap();
    assert_eq!(staking_link_initiate.risk, Risk::FundsMovement);
    assert_eq!(staking_link_initiate.dry_run, DryRunPolicy::Optional);
    assert_eq!(
        staking_link_initiate.confirmation,
        ConfirmationPolicy::Prompt
    );
    assert!(staking_link_initiate.auth_required);

    for path in [
        &["vault", "deposit"][..],
        &["vault", "withdraw"][..],
        &["borrowlend", "supply"][..],
        &["borrowlend", "withdraw"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.risk, Risk::FundsMovement, "{path:?}");
        assert_eq!(command.dry_run, DryRunPolicy::Optional, "{path:?}");
        assert!(command.auth_required, "{path:?}");
    }

    for path in [
        &["builder", "approve"][..],
        &["referral", "set"][..],
        &["referral", "register"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.risk, Risk::FundsMovement, "{path:?}");
        assert_eq!(command.dry_run, DryRunPolicy::Optional, "{path:?}");
        assert!(command.auth_required, "{path:?}");
    }

    for path in [
        &["subaccount", "create"][..],
        &["subaccount", "transfer"][..],
        &["subaccount", "spot-transfer"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.risk, Risk::FundsMovement, "{path:?}");
        assert_eq!(command.dry_run, DryRunPolicy::Optional, "{path:?}");
        assert!(command.auth_required, "{path:?}");
    }

    let subaccount_transfer = registry.find_path(&["subaccount", "transfer"]).unwrap();
    let subaccount_input = subaccount_transfer
        .inputs
        .iter()
        .find(|input| input.id == "subaccount")
        .unwrap();
    assert_eq!(
        subaccount_input.kind,
        Some(InputKind::ActingAccountSelector)
    );

    let builder_approve = registry.find_path(&["builder", "approve"]).unwrap();
    let builder_input = builder_approve
        .inputs
        .iter()
        .find(|input| input.id == "builder")
        .unwrap();
    assert_eq!(builder_input.kind, Some(InputKind::ProtocolObjectAddress));
}

#[test]
fn authenticated_reads_do_not_inherit_mutating_preview_policy() {
    let registry = CommandRegistry::get();

    for path in [
        &["api-wallet", "list"][..],
        &["orders", "open"][..],
        &["orders", "history"][..],
        &["positions", "list"][..],
        &["referral", "status"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.lifecycle, Lifecycle::ReadOnly, "{path:?}");
        assert_eq!(command.risk, Risk::AccountState, "{path:?}");
        assert_eq!(command.dry_run, DryRunPolicy::NotSupported, "{path:?}");
        assert_eq!(
            command.raw_payload,
            RawPayloadPolicy::Unsupported,
            "{path:?}"
        );
    }

    for path in [
        &["subscribe", "order-updates"][..],
        &["subscribe", "fills"][..],
    ] {
        let command = registry.find_path(path).unwrap();
        assert_eq!(command.lifecycle, Lifecycle::Streaming, "{path:?}");
        assert_eq!(command.risk, Risk::AccountState, "{path:?}");
        assert_eq!(command.dry_run, DryRunPolicy::NotSupported, "{path:?}");
        assert_eq!(
            command.raw_payload,
            RawPayloadPolicy::Unsupported,
            "{path:?}"
        );
    }
}
