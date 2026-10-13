//! Live data source adapters.
//!
//! A [`Source`] is an async task that pushes [`Sample`]s into the engine. The
//! [`Registry`] maps a scene's `SourceSpec.kind` to a factory. Other crates
//! (e.g. `sonder-vision`) register additional kinds via [`Registry::register`].

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sonder_core::{Clock, Sample, SourceId, SourceSpec};

pub mod registry;
pub use registry::{Registry, SourceInfo, ParamInfo, Factory};

/// Everything a running source needs.
#[derive(Clone)]
pub struct SourceCtx {
    pub id: SourceId,
    pub tx: tokio::sync::mpsc::Sender<Sample>,
    pub clock: Arc<dyn Clock>,
    pub http: reqwest::Client,
    /// Becomes `true` when the engine is shutting down.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    /// Seed for synthetic sources (deterministic renders).
    pub seed: u64,
}

impl SourceCtx {
    /// Current engine time.
    pub fn now(&self) -> sonder_core::Seconds {
        self.clock.now()
    }
    /// Send a sample; returns false if the engine has gone away.
    pub async fn emit(&self, sample: Sample) -> bool {
        self.tx.send(sample).await.is_ok()
    }
    /// True once shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }
    /// Resolves when shutdown is requested.
    pub async fn wait_shutdown(&mut self) {
        while !*self.shutdown.borrow() {
            if self.shutdown.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A boxed future with no output, `Send`.
pub type SourceFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A live data producer. `run` owns the task until shutdown.
pub trait Source: Send {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture;
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("unknown source kind `{0}`")]
    UnknownKind(String),
    #[error("source `{kind}`: missing required parameter `{param}`")]
    MissingParam { kind: String, param: &'static str },
    #[error("source `{kind}`: invalid parameter `{param}`: {message}")]
    InvalidParam { kind: String, param: &'static str, message: String },
    #[error("{0}")]
    Other(String),
}

/// Convenience for factories.
pub fn require_str<'a>(spec: &'a SourceSpec, param: &'static str) -> Result<&'a str, SourceError> {
    spec.get_str(param).ok_or_else(|| SourceError::MissingParam { kind: spec.kind.clone(), param })
}

/// Registers all builtin kinds. Implemented in the adapter modules.
pub fn register_builtins(_r: &mut Registry) {
    // filled in by the adapters (synthetic, system, clock, wikipedia, usgs, …)
}
