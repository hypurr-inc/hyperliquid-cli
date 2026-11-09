# Contract Fixtures

Snapshots of what the CLI tells an agent, so drift fails a test:

- `registry_inventory.json` — every command's `CommandContract` serialized: the
  value `hyperliquid schema` prints for it, minus the derived `json_schema`.
  It carries the policy of each command (risk, dry-run, confirmation, signer
  support, raw payload, input kinds, output contract), so a change to any of
  them shows up here.
- `schema_representative.json` — the full schema, `json_schema` included, of six
  commands.
- `dry_run_outputs.json` — the `--dry-run` envelopes of two order commands.
- `output_error_envelope.json` — the JSON envelope of a usage error.
- `output_projection.json` — `--results-only --max-results --select` on a list.

The first two are generated from the command registry by `registry_fixtures()`
in `tests/contract_support/mod.rs` and checked by `tests/contract_fixtures.rs`;
the others record what the binary prints.

To update them intentionally:

```bash
HYPERLIQUID_UPDATE_CONTRACTS=1 cargo test --test contract_fixtures --test dry_run_contracts --test output_contracts
```

Review the resulting diff. Do not update snapshots as a side effect of unrelated
feature work. `HYPERLIQUID_UPDATE_CONTRACTS=1` is the one switch for every
snapshot in the repository.
