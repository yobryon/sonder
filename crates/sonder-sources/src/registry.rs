//! Source catalog and factory registry.

use sonder_core::SourceSpec;

use crate::{Source, SourceError};

/// Describes a parameter a source kind accepts (for `sonder sources` and `probe`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamInfo {
    pub name: &'static str,
    pub summary: &'static str,
    /// Rendered default, or None if required.
    pub default: Option<&'static str>,
}

/// Describes a source kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// e.g. "usgs.earthquakes"
    pub kind: &'static str,
    pub summary: &'static str,
    pub params: Vec<ParamInfo>,
    /// Reading channel names it emits.
    pub channels: Vec<&'static str>,
    /// Event names it emits.
    pub events: Vec<&'static str>,
    /// Needs network access.
    pub network: bool,
}

pub type Factory = fn(&SourceSpec) -> Result<Box<dyn Source>, SourceError>;

/// Maps `SourceSpec.kind` → factory.
#[derive(Default)]
pub struct Registry {
    entries: Vec<(SourceInfo, Factory)>,
}

impl Registry {
    /// Empty registry.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Registry with every builtin kind in this crate.
    pub fn new() -> Self {
        let mut r = Self::empty();
        crate::register_builtins(&mut r);
        r
    }

    pub fn register(&mut self, info: SourceInfo, factory: Factory) {
        self.entries.retain(|(i, _)| i.kind != info.kind);
        self.entries.push((info, factory));
    }

    pub fn create(&self, spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
        let kind = spec.kind.trim();
        self.entries
            .iter()
            .find(|(i, _)| i.kind == kind)
            .ok_or_else(|| SourceError::UnknownKind(spec.kind.clone()))
            .and_then(|(_, f)| f(spec))
    }

    pub fn info(&self, kind: &str) -> Option<&SourceInfo> {
        self.entries.iter().find(|(i, _)| i.kind == kind).map(|(i, _)| i)
    }

    pub fn catalog(&self) -> Vec<&SourceInfo> {
        let mut v: Vec<&SourceInfo> = self.entries.iter().map(|(i, _)| i).collect();
        v.sort_by_key(|i| i.kind);
        v
    }
}
