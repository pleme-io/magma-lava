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
