//! End-to-end integration tests for magma-lava — the operational
//! surface magma's plan/apply pipeline consumes.

use magma_lava::{synthesize, typed_interface, LavaError, LavaPlanArgs};

/// Write a .tlisp source to a tempdir and synthesize it via the
/// public entry point. Mirrors what `magma plan --tlisp foo.tlisp`
/// will do under the hood.
fn tempfile_with(contents: &str, name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "magma-lava-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.tlisp"));
    std::fs::write(&path, contents).unwrap();
    path
}

const VPC_TLISP: &str = r#"
(deflava-architecture demo-vpc
  :inputs ((:cidr "10.0.0.0/16"))
  :resources (
    (aws-vpc "demo"
      :cidr-block "{cidr}"
      :enable-dns-support #t)))
"#;

#[test]
fn synthesize_resolves_runtime_evaluates_and_renders_terraform_json() {
    let path = tempfile_with(VPC_TLISP, "demo-vpc");
    let plan = synthesize(&LavaPlanArgs::for_path(&path)).unwrap();

    assert_eq!(plan.runtime_kind, "lava");
    assert_eq!(
        plan.terraform_json["resource"]["aws_vpc"]["demo"]["cidr_block"],
        "10.0.0.0/16"
    );
    assert_eq!(
        plan.terraform_json["resource"]["aws_vpc"]["demo"]["enable_dns_support"],
        true
    );
}

#[test]
fn synthesize_threads_scalar_bindings_through_eval() {
    let path = tempfile_with(VPC_TLISP, "demo-vpc");
    let args = LavaPlanArgs::for_path(&path).with_scalar("cidr", "172.20.0.0/16");
    let plan = synthesize(&args).unwrap();
    assert_eq!(
        plan.terraform_json["resource"]["aws_vpc"]["demo"]["cidr_block"],
        "172.20.0.0/16"
    );
}

#[test]
fn synthesize_returns_typed_io_error_for_missing_file() {
    let args = LavaPlanArgs::for_path("/tmp/does-not-exist-magma-lava.tlisp");
    let err = synthesize(&args).unwrap_err();
    assert!(matches!(err, LavaError::Io { .. }));
}

#[test]
fn synthesize_returns_typed_no_runtime_for_unknown_extension() {
    let path = tempfile_with("not used", "demo-vpc");
    let yaml = path.with_extension("yaml");
    std::fs::write(&yaml, "key: value").unwrap();
    let args = LavaPlanArgs::for_path(&yaml);
    let err = synthesize(&args).unwrap_err();
    assert!(matches!(err, LavaError::NoRuntime { .. }));
}

#[test]
fn synthesize_gated_by_bundled_interface_accepts_valid_input() {
    let path = tempfile_with(VPC_TLISP, "demo-vpc");
    let plan = synthesize(
        &LavaPlanArgs::for_path(&path)
            .with_scalar("cidr", "10.0.0.0/16")
            .gated_by("aws-vpc-network"),
    );
    // aws-vpc-network gate is permissive on cidr (Field::optional);
    // we expect the gate to pass because :cidr is acceptable.
    assert!(plan.is_ok(), "gate should accept default cidr: {plan:?}");
}

#[test]
fn synthesize_gated_by_unknown_interface_returns_typed_error() {
    let path = tempfile_with(VPC_TLISP, "demo-vpc");
    let args = LavaPlanArgs::for_path(&path).gated_by("not-a-real-interface");
    let err = synthesize(&args).unwrap_err();
    match err {
        LavaError::UnknownInterface(name) => assert_eq!(name, "not-a-real-interface"),
        other => panic!("expected UnknownInterface, got {other:?}"),
    }
}

#[test]
fn synthesize_gated_by_cloudflare_rejects_missing_required_input() {
    // cloudflare-dns-records requires :zone-id. Use the bundled
    // architecture's source (round-trips through the registry path).
    let src = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("lava-architectures")
            .join("architectures")
            .join("cloudflare-dns-records.tlisp"),
    );
    if let Ok(src) = src {
        let path = tempfile_with(&src, "cloudflare-dns-records");
        let args = LavaPlanArgs::for_path(&path).gated_by("cloudflare-dns-records");
        let err = synthesize(&args).unwrap_err();
        match err {
            LavaError::Runtime(lava_runtime::RuntimeError::Schema { interface, .. }) => {
                assert_eq!(interface, "cloudflare-dns-records");
            }
            other => panic!("expected RuntimeError::Schema, got {other:?}"),
        }
    }
}

#[test]
fn typed_interface_lookup_returns_bundled_entries() {
    assert!(typed_interface("aws-vpc-network").is_some());
    assert!(typed_interface("cloudflare-dns-records").is_some());
    assert!(typed_interface("akeyless-secrets").is_some());
    assert!(typed_interface("nope-never-existed").is_none());
}

#[test]
fn lava_plan_emits_crossplane_yaml_for_same_architecture() {
    let path = tempfile_with(VPC_TLISP, "demo-vpc");
    let plan = synthesize(&LavaPlanArgs::for_path(&path)).unwrap();
    let yaml = plan.crossplane_yaml().unwrap();
    // Same Architecture flows through to the second target.
    assert!(yaml.contains("kind: CompositeResourceDefinition"));
    assert!(yaml.contains("kind: Composition"));
    assert!(yaml.contains("cidr_block: 10.0.0.0/16"));
}

#[test]
fn synthesize_records_runtime_kind_for_plan_receipt() {
    let path = tempfile_with(VPC_TLISP, "demo-vpc");
    let plan = synthesize(&LavaPlanArgs::for_path(&path)).unwrap();
    assert_eq!(plan.runtime_kind, "lava");
    assert!(plan.diagnostics.is_empty()); // happy path = silent
}
