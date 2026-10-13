//! `stdin` — JSON lines on standard input; each object becomes a reading + `line` event.

use sonder_core::{SourceSpec, SourceState};
use tokio::io::AsyncBufReadExt;

use crate::adapters::file_tail::{Format, LineConverter};
use crate::adapters::generic::{EventsOn, GenericConfig};
use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

struct StdinSource {
    format: Format,
    cfg: GenericConfig,
}

impl Source for StdinSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut lc = LineConverter::new(self.format, self.cfg);
            em.status(SourceState::Idle, "reading stdin").await;
            let stdin = tokio::io::stdin();
            let mut lines = tokio::io::BufReader::new(stdin).lines();
            loop {
                let next = tokio::select! {
                    l = lines.next_line() => l,
                    _ = em.ctx.wait_shutdown() => return,
                };
                match next {
                    Ok(Some(line)) => {
                        let data = lc.line(&line);
                        if !data.is_empty() {
                            em.status(SourceState::Live, "live").await;
                        }
                        for d in data {
                            em.datum(d).await;
                        }
                    }
                    Ok(None) => {
                        em.status(SourceState::Idle, "stdin closed").await;
                        em.ctx.wait_shutdown().await;
                        return;
                    }
                    Err(e) => {
                        em.status(SourceState::Failed, format!("stdin error: {e}")).await;
                        em.ctx.wait_shutdown().await;
                        return;
                    }
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "stdin",
        summary: "JSON lines from standard input: numeric fields → readings, each line → `line` event",
        params: vec![
            ParamInfo { name: "format", summary: "jsonl | csv | lines", default: Some("jsonl") },
            ParamInfo { name: "fields", summary: "Paths or \"auto\"", default: Some("auto") },
            ParamInfo { name: "events_on", summary: "change | every | none", default: Some("every") },
            ParamInfo { name: "event_name", summary: "Name of the emitted event", default: Some("line") },
        ],
        channels: vec!["<numeric fields>"],
        events: vec!["line"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let format = Format::parse(&util::param_str(spec, "format", "jsonl"))
        .ok_or_else(|| util::invalid(spec, "format", "expected jsonl | csv | lines"))?;
    Ok(Box::new(StdinSource { format, cfg: GenericConfig::from_spec(spec, EventsOn::Every, "line")? }))
}
