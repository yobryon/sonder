//! Static frontend files, embedded at compile time so the binary is self-contained.

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

pub(crate) const INDEX_HTML: &str = include_str!("../static/index.html");
pub(crate) const APP_JS: &str = include_str!("../static/app.js");
pub(crate) const APP_CSS: &str = include_str!("../static/app.css");
pub(crate) const WORKLET_JS: &str = include_str!("../static/worklet.js");
pub(crate) const TRANSPORT_JS: &str = include_str!("../static/transport.js");

const JS: &str = "text/javascript; charset=utf-8";
const JSON: &str = "application/json; charset=utf-8";

/// Every file the UI needs in `ws` mode: (route, body, content type). `app.js` imports the
/// helper modules by relative path, so each must be served next to it.
pub(crate) const FILES: &[(&str, &str, &str)] = &[
    ("/index.html", INDEX_HTML, "text/html; charset=utf-8"),
    ("/app.js", APP_JS, JS),
    ("/app.css", APP_CSS, "text/css; charset=utf-8"),
    ("/worklet.js", WORKLET_JS, JS),
    ("/transport.js", TRANSPORT_JS, JS),
    ("/scene-url.js", include_str!("../static/scene-url.js"), JS),
    ("/captions.js", include_str!("../static/captions.js"), JS),
    ("/roll.js", include_str!("../static/roll.js"), JS),
    ("/geo.js", include_str!("../static/geo.js"), JS),
    ("/editor.js", include_str!("../static/editor.js"), JS),
    ("/coastlines.json", include_str!("../static/coastlines.json"), JSON),
];

fn static_response(body: &'static str, content_type: &'static str) -> Response {
    ([(CONTENT_TYPE, content_type), (CACHE_CONTROL, "no-cache")], body).into_response()
}

pub(crate) async fn index() -> Response {
    static_response(INDEX_HTML, "text/html; charset=utf-8")
}

/// Add a route per embedded file.
pub(crate) fn routes<S: Clone + Send + Sync + 'static>(mut router: Router<S>) -> Router<S> {
    for &(path, body, ct) in FILES {
        router = router.route(path, get(move || async move { static_response(body, ct) }));
    }
    router
}
