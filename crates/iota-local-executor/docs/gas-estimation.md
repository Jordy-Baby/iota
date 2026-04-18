# Gas estimation

`GasEstimate` turns the Move VM's gas ledger from a simulation's effects into a flat struct convenient for picking a gas budget for the real submission.

## The numbers

Every `TransactionEffects` carries a `GasCostSummary`:

- `computation_cost` — gas charged for CPU work.
- `storage_cost` — gas charged for writing objects.
- `storage_rebate` — gas refunded for freeing objects.
- `non_refundable_storage_fee` — portion of storage cost that's not rebatable.
- `net_gas_usage = computation + storage - rebate` — the figure a budget needs to cover.

`GasEstimate::from_effects` extracts all five.

## Usage

```rust
use iota_local_executor::GasEstimate;

let result = executor.simulate_transaction(tx, VmChecks::Enabled).await?;
let estimate = GasEstimate::from_effects(&result.effects);

println!("computation: {}", estimate.computation_cost);
println!("storage:     {}", estimate.storage_cost);
println!("rebate:      {}", estimate.storage_rebate);
println!("net:         {}", estimate.net_gas_usage);
```

## Budgeting the real submission

`suggested_budget_with_headroom(factor)` multiplies `net_gas_usage` by `factor` and rounds up. 20% headroom (`1.2`) is a reasonable default — it covers small variations in computation across epochs without wasting gas. Returns `0` for rebate-heavy transactions whose net is non-positive:

```rust
let budget = estimate.suggested_budget_with_headroom(1.2);

let tx = TransactionData::new_programmable(
    sender,
    gas_payment,
    pt,
    budget,       // ← use the estimated budget
    gas_price,
);
```

## Caveat

Local gas numbers match the production VM exactly for the same `ProtocolConfig`, so this is reliable for budgeting. The one thing that can drift is the **reference gas price**: if the network's RGP changes between simulation and submission, adjust with headroom. The RGP is captured in `ChainInfo::reference_gas_price` — see [remote-caching.md](details/remote-caching.md).

## Examples and tests

- [../tests/event_decoding.rs::gas_estimate_surfaces_effects_numbers](../tests/event_decoding.rs)
- [../src/gas.rs](../src/gas.rs)
