//! Static frontend files, embedded at compile time so the binary is self-contained.

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};

pub(crate) const INDEX_HTML: &str = include_str!("../static/index.html");
pub(crate) const APP_JS: &str = include_str!("../static/app.js");
pub(crate) const APP_CSS: &str = include_str!("../static/app.css");
pub(crate) const WORKLET_JS: &str = include_str!("../static/worklet.js");
pub(crate) const TRANSPORT_JS: &str = include_str!("../static/transport.js");

fn static_response(body: &'static str, content_type: &'static str) -> Response {
    ([(CONTENT_TYPE, content_type), (CACHE_CONTROL, "no-cache")], body).into_response()
}

pub(crate) async fn index() -> Response {
    static_response(INDEX_HTML, "text/html; charset=utf-8")
}

pub(crate) async fn app_js() -> Response {
    static_response(APP_JS, "text/javascript; charset=utf-8")
}

pub(crate) async fn app_css() -> Response {
    static_response(APP_CSS, "text/css; charset=utf-8")
}

pub(crate) async fn worklet_js() -> Response {
    static_response(WORKLET_JS, "text/javascript; charset=utf-8")
}

pub(crate) async fn transport_js() -> Response {
    static_response(TRANSPORT_JS, "text/javascript; charset=utf-8")
}
