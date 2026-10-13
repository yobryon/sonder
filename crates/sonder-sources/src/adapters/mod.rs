//! Builtin source adapters, one module per kind. Each exports
//! `info() -> SourceInfo` and `factory(&SourceSpec) -> Result<Box<dyn Source>>`.

pub mod astro;
pub mod coinbase;
pub mod file_tail;
pub mod freeway;
pub mod generic;
pub mod github;
pub mod hackernews;
pub mod http_json;
pub mod iss;
pub mod open_meteo;
pub mod opensky;
pub mod space_weather;
pub mod sse;
pub mod stdin;
pub mod system;
pub mod tides;
pub mod usgs;
pub mod walk;
pub mod websocket;
pub mod wikipedia;
