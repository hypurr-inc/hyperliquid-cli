# Contract Fixtures

These fixtures are characterization snapshots for issues #42/#43. They freeze
the current CLI/schema/registry/dry-run/output contracts so drift fails in
tests before the command registry migration changes architecture.

`command_inventory.json`, `registry_inventory.json`, `schema_high_risk_metadata.json`
and `schema_representative.json` are all generated from the command registry by
`registry_fixtures()` in `tests/contract_support/mod.rs`; `tests/contract_fixtures.rs`
checks them. `registry_inventory.json` is the registry's `CommandContract`s
serialized: the same value `hyperliquid schema` prints for each command, minus
its derived `json_schema`. `dry_run_outputs.json`, `output_error_envelope.json` and
`output_projection.json` record what the binary prints.

To update them intentionally:

```bash
HYPERLIQUID_UPDATE_CONTRACTS=1 cargo test --test contract_fixtures --test dry_run_contracts --test output_contracts
```

Review the resulting diff. Do not update snapshots as a side effect of unrelated
feature work.
