//! magma-lava — the façade crate magma consumes to load lava
//! architectures (.tlisp) directly.
//!
//! Single entry point: [`synthesize`] takes a [`LavaPlanArgs`] and
//! returns a typed terraform.json value magma's existing plan/apply
//! pipeline consumes unchanged.
//!
//! ## Pipeline
//!
//! ```text
//! .tlisp file path + bindings + optional schema name
//!     │
//!     ▼  pick_runtime_for_path / LavaRuntime::evaluate_with_schema
//! lava_core::Architecture
//!     │
//!     ▼  Synthesizer<TerraformJson>
//! serde_json::Value (terraform.json shape)
//!     │
//!     ▼  magma::plan(json) / magma::apply(json)
//! cloud state
//! ```
//!
//! ## Zero-disk-roundtrip discipline
//!
//! No `.tlisp` ever gets compiled to a temporary `.tf.json` file on
//! disk. The Architecture lives in memory; the JSON value lives in
//! memory; magma's plan engine reads the in-memory shape. The only
//! file system reads are: (a) the source `.tlisp` (b) optional
//! `--binding key=value` overlays a CLI might layer on.

#![allow(clippy::module_name_repetitions)]

use indexmap::IndexMap;
use lava_core::{Architecture, CrossplaneYaml, Synthesizer, TerraformJson};
use lava_runtime::{
    pick_runtime_for_path, ArtifactBinding, EmbeddedRuntime, EvaluationResult, Interface,
    RuntimeError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use lava_architectures::interface_for as bundled_interface_for;
pub use lava_core::{MagmaPlan, Resource, ResourceRef, Value};
pub use lava_runtime::{ArtifactBinding as Binding, RuntimeError as RtError};
pub use lava_schema::{Field, Interface as TypedInterface, SchemaError};

/// Typed args for one synthesize call. Mirrors the magma CLI
/// surface: `magma plan --tlisp <path> --binding k=v --gate <iface>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LavaPlanArgs {
    /// Path to the `.tlisp` source file. Required.
    pub path: std::path::PathBuf,
    /// Operator-supplied input bindings; mapped 1:1 onto the .tlisp
    /// architecture's `:inputs` slot.
    pub bindings: IndexMap<String, ArtifactBinding>,
    /// Optional schema gate. If `Some(name)`, the matching bundled
    /// interface (from lava-architectures' registry) is fetched and
    /// the bindings are validated against it before evaluation.
    pub gate_with: Option<String>,
    /// Optional override of the runtime selection. When `None`,
    /// `pick_runtime_for_path` infers the runtime from the file
    /// extension. Useful for `magma plan --runtime ruby foo.txt`.
    pub runtime_kind: Option<String>,
}

impl LavaPlanArgs {
    /// Convenience constructor for the most common case: just a path.
    #[must_use]
    pub fn for_path(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            bindings: IndexMap::new(),
            gate_with: None,
            runtime_kind: None,
        }
    }

    /// Set a scalar binding on the args.
    #[must_use]
    pub fn with_scalar(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.bindings
            .insert(k.into(), ArtifactBinding::Scalar(v.into()));
        self
    }

    /// Set a list binding on the args.
    #[must_use]
    pub fn with_list(mut self, k: impl Into<String>, v: Vec<String>) -> Self {
        self.bindings
            .insert(k.into(), ArtifactBinding::List(v));
        self
    }

    /// Gate the architecture by a bundled interface name.
    #[must_use]
    pub fn gated_by(mut self, name: impl Into<String>) -> Self {
        self.gate_with = Some(name.into());
        self
    }
}

/// Typed result — the rendered terraform.json + the typed
/// Architecture + diagnostics + the runtime kind that produced it.
#[derive(Debug, Clone)]
pub struct LavaPlan {
    pub terraform_json: serde_json::Value,
    pub architecture: Architecture,
    pub runtime_kind: String,
    pub diagnostics: Vec<lava_runtime::Diagnostic>,
}

impl LavaPlan {
    /// Lazy re-render to Crossplane YAML (XRD + Composition pair).
    /// Same Architecture, second target — proves the multi-renderer
    /// pattern composes without extra plumbing.
    ///
    /// # Errors
    /// Bubbles up [`LavaError::Render`] when the typed renderer fails.
    pub fn crossplane_yaml(&self) -> Result<String, LavaError> {
        Synthesizer::<CrossplaneYaml>::synthesize(&self.architecture)
            .map_err(|e| LavaError::Render(e.to_string()))
    }
}

#[derive(Debug, Error)]
pub enum LavaError {
    #[error("io reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no runtime matches path {path} (extension hint resolved to nothing)")]
    NoRuntime { path: std::path::PathBuf },
    #[error("unknown bundled interface `{0}` — not in lava-architectures::interface_for")]
    UnknownInterface(String),
    #[error("runtime: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("render terraform.json: {0}")]
    Render(String),
}

/// The single entry point magma calls. Loads + evaluates + (optionally)
/// schema-gates + synthesizes to terraform.json. Returns everything
/// magma needs to drive plan/apply.
///
/// # Errors
/// See [`LavaError`] — wraps every upstream failure mode in one typed
/// enum.
pub fn synthesize(args: &LavaPlanArgs) -> Result<LavaPlan, LavaError> {
    // Probe readability up front so a missing file surfaces as the
    // typed LavaError::Io variant the caller can match on (the
    // runtime's path helper would surface this as RuntimeError::Io,
    // which is less actionable for CLI output).
    if !args.path.exists() {
        return Err(LavaError::Io {
            path: args.path.clone(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
    }

    let rt = select_runtime(args)?;

    let result: EvaluationResult = match &args.gate_with {
        Some(iface_name) => {
            let iface = bundled_interface_for(iface_name)
                .ok_or_else(|| LavaError::UnknownInterface(iface_name.clone()))?;
            rt.evaluate_path_with_schema(&args.path, args.bindings.clone(), &iface)?
        }
        None => rt.evaluate_path(&args.path, args.bindings.clone())?,
    };

    let tf = Synthesizer::<TerraformJson>::synthesize(&result.architecture)
        .map_err(|e| LavaError::Render(e.to_string()))?;

    Ok(LavaPlan {
        terraform_json: tf,
        architecture: result.architecture,
        runtime_kind: rt.kind().to_string(),
        diagnostics: result.diagnostics,
    })
}

/// Standalone schema lookup — useful for CLI surfaces that want to
/// list available bundled interfaces before the operator picks one.
#[must_use]
pub fn typed_interface(name: &str) -> Option<TypedInterface> {
    bundled_interface_for(name)
}

/// Typed source descriptor for [`synthesize_source`] — the
/// operator-facing entry-point that mirrors lava-operator's
/// `LavaArchitectureSpec.source` shape. Lets in-cluster controllers
/// (kube-rs, cron-trigger, ad-hoc CLI scripts) drive magma-lava
/// without first materializing the source to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LavaSource {
    /// .tlisp source text embedded directly.
    Inline { inline: String },
    /// Bundled architecture name (resolved via
    /// `lava-architectures::load_bundled`).
    Bundled { name: String },
    /// On-disk path (used by the CLI; mirrors the original
    /// [`LavaPlanArgs::path`] field).
    Path { path: std::path::PathBuf },
}

/// Source-aware façade. Dispatches to the path / inline / bundled
/// pipeline based on the typed `source`. Consumers compose this with
/// magma's plan/apply trivially:
///
/// ```ignore
/// let plan = magma_lava::synthesize_source(&source, &bindings, gate)?;
/// let cfg  = magma::config::parse(plan.terraform_json)?;
/// let p    = magma_plan::plan(&cfg, &state)?;
/// magma::apply::apply(&p, &mut state)?;
/// ```
///
/// # Errors
/// Returns [`LavaError`] for every upstream failure mode.
pub fn synthesize_source(
    source: &LavaSource,
    bindings: &IndexMap<String, ArtifactBinding>,
    gate_with: Option<&str>,
) -> Result<LavaPlan, LavaError> {
    match source {
        LavaSource::Path { path } => {
            let mut args = LavaPlanArgs::for_path(path.clone());
            args.bindings = bindings.clone();
            args.gate_with = gate_with.map(std::string::ToString::to_string);
            synthesize(&args)
        }
        LavaSource::Inline { inline } => synthesize_inline(inline, bindings.clone(), gate_with),
        LavaSource::Bundled { name } => synthesize_bundled(name, bindings.clone(), gate_with),
    }
}

/// Inline-source pipeline. Runs the .tlisp runtime against a
/// string instead of a file — keeps in-cluster controllers
/// disk-free.
///
/// # Errors
/// See [`LavaError`].
pub fn synthesize_inline(
    source: &str,
    bindings: IndexMap<String, ArtifactBinding>,
    gate_with: Option<&str>,
) -> Result<LavaPlan, LavaError> {
    let rt = lava_runtime::LavaRuntime::new();
    let input = lava_runtime::ArtifactInput {
        source: source.to_string(),
        bindings,
        name: None,
    };
    let result = match gate_with {
        Some(name) => {
            let iface = bundled_interface_for(name)
                .ok_or_else(|| LavaError::UnknownInterface(name.to_string()))?;
            rt.evaluate_with_schema(&input, &iface)?
        }
        None => rt.evaluate(&input)?,
    };
    let tf = Synthesizer::<TerraformJson>::synthesize(&result.architecture)
        .map_err(|e| LavaError::Render(e.to_string()))?;
    Ok(LavaPlan {
        terraform_json: tf,
        architecture: result.architecture,
        runtime_kind: rt.kind().to_string(),
        diagnostics: result.diagnostics,
    })
}

/// Bundled-architecture pipeline. Looks up the named architecture in
/// lava-architectures' embedded registry, applies bindings + optional
/// schema gate, and synthesizes.
///
/// # Errors
/// Returns [`LavaError::UnknownInterface`] when the gate name doesn't
/// resolve; surfaces upstream eval errors via [`LavaError::Runtime`].
pub fn synthesize_bundled(
    name: &str,
    bindings: IndexMap<String, ArtifactBinding>,
    gate_with: Option<&str>,
) -> Result<LavaPlan, LavaError> {
    let source = lava_architectures::bundled_source(name).ok_or(LavaError::UnknownInterface(
        format!("unknown bundled architecture `{name}`"),
    ))?;
    synthesize_inline(&source, bindings, gate_with)
}

fn select_runtime(args: &LavaPlanArgs) -> Result<Box<dyn EmbeddedRuntime>, LavaError> {
    if let Some(kind) = &args.runtime_kind {
        // Explicit kind overrides extension-based inference. Currently
        // only `lava` / `terraform-json` are wired here; future
        // ruby/tatara-script runtimes will land in this match.
        return match kind.as_str() {
            "lava" => Ok(Box::new(lava_runtime::LavaRuntime::new())),
            "terraform-json" => Ok(Box::new(lava_runtime::TerraformJsonRuntime::new())),
            other => Err(LavaError::NoRuntime {
                path: std::path::PathBuf::from(format!("(runtime_kind={other})")),
            }),
        };
    }
    pick_runtime_for_path(&args.path).ok_or_else(|| LavaError::NoRuntime {
        path: args.path.clone(),
    })
}

// ── Magma planning bridge (L3.1 / G4) ─────────────────────────────

/// One typed planned change extracted from magma's `Plan`. Mirrors
/// magma's `ResourceChange` shape but normalised onto the
/// per-attribute granularity drift consumers expect.
///
/// `magma_types::Action::*` collapses to a stable string kind so
/// downstream consumers (lava-drift, lava-anomaly) can match without
/// pulling magma-types into their own dep graphs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlannedChange {
    pub address: String,
    pub attribute: String,
    /// `"create" | "update" | "delete" | "replace" | "no_op" | "read"`.
    pub kind: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Synthesize a [`LavaSource`] + run magma's plan engine against an
/// empty `State` (treat first plan as baseline). Returns one
/// [`PlannedChange`] per resource address × notable-attribute pair.
///
/// Used by lava-operator's `CallbackPlanner` as the typed magma seam
/// — keeps the drift detector ignorant of magma internals.
///
/// # Errors
/// Returns [`LavaError`] for synthesize failures and
/// [`LavaError::Render`] for magma-config parse / magma-plan
/// failures (collapsed under one variant to keep the public surface
/// narrow).
pub fn plan_changes(
    source: &LavaSource,
    bindings: &IndexMap<String, ArtifactBinding>,
    gate_with: Option<&str>,
) -> Result<Vec<PlannedChange>, LavaError> {
    let plan = synthesize_source(source, bindings, gate_with)?;
    let cfg = magma_config::Config::from_json(plan.terraform_json)
        .map_err(|e| LavaError::Render(format!("magma-config parse: {e}")))?;
    let state = magma_types::State {
        version: 4,
        terraform_version: "1.5.0".to_string(),
        serial: 0,
        lineage: uuid::Uuid::nil(),
        outputs: std::collections::HashMap::new(),
        resources: Vec::new(),
    };
    let typed_plan = magma_plan::plan(&cfg, &state)
        .map_err(|e| LavaError::Render(format!("magma plan: {e}")))?;
    Ok(typed_plan
        .resource_changes
        .into_iter()
        .map(planned_change_from_resource_change)
        .collect())
}

fn planned_change_from_resource_change(rc: magma_types::ResourceChange) -> PlannedChange {
    let address = format!("{}.{}", rc.address.type_id.0, rc.address.name);
    let kind = match rc.action {
        magma_types::Action::NoOp => "no_op",
        magma_types::Action::Create => "create",
        magma_types::Action::Read => "read",
        magma_types::Action::Update => "update",
        magma_types::Action::Replace => "replace",
        magma_types::Action::Delete => "delete",
        magma_types::Action::Forget => "delete",
        magma_types::Action::CreateThenDelete => "replace",
        magma_types::Action::DeleteThenCreate => "replace",
    };
    PlannedChange {
        address,
        attribute: "*".to_string(),
        kind: kind.to_string(),
        before: rc.before.map(|v| v.to_string()),
        after: rc.after.map(|v| v.to_string()),
    }
}
