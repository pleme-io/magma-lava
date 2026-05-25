# magma-lava

The façade crate magma consumes to load lava architectures (`.tlisp`)
directly. Single entry point:

```rust
use magma_lava::{synthesize, LavaPlanArgs};

let args = LavaPlanArgs::for_path("architectures/aws-vpc-network.tlisp")
    .with_scalar("name", "preview")
    .with_list("availability-zones", vec!["us-west-2a".into(), "us-west-2b".into()])
    .gated_by("aws-vpc-network");

let plan = synthesize(&args)?;
// plan.terraform_json is magma-compatible; plan/apply consume it.
```

## Pipeline

```text
.tlisp file path + bindings + optional schema name
    │
    ▼  pick_runtime_for_path / LavaRuntime::evaluate_with_schema
lava_core::Architecture
    │
    ▼  Synthesizer<TerraformJson>
serde_json::Value
    │
    ▼  magma::plan(json) / magma::apply(json)
cloud state
```

## Errors

[`LavaError`](src/lib.rs) wraps every upstream failure mode in one
typed enum: `Io` / `NoRuntime` / `UnknownInterface` / `Runtime` /
`Render`.

## Tests

`cargo test --release` runs the end-to-end integration suite —
synthesize-from-tempfile, scalar/list overrides, schema gate
accept/reject, typed-error rounds.
