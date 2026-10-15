//! `seedlink` — stub; filled in by its implementer.

use sonder_core::SourceSpec;

use crate::registry::SourceInfo;
use crate::{Source, SourceError};

pub fn info() -> SourceInfo {
    SourceInfo { kind: "seedlink", summary: "(not yet implemented)", params: vec![], channels: vec![], events: vec![], network: true }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Err(SourceError::Other(format!("source kind `{}` is not implemented yet", spec.kind)))
}
