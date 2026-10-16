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
    ("/timeline.js", include_str!("../static/timeline.js"), JS),
    ("/coastlines.json", include_str!("../static/coastlines.json"), JSON),
    ("/compose.js", include_str!("../static/compose.js"), JS),
    ("/compose.css", include_str!("../static/compose.css"), "text/css; charset=utf-8"),
    ("/pwa.js", include_str!("../static/pwa.js"), JS),
    ("/manifest.webmanifest", include_str!("../static/manifest.webmanifest"), "application/manifest+json"),
    ("/icons/icon.svg", include_str!("../static/icons/icon.svg"), "image/svg+xml"),
];

/// Binary assets (icons) for `ws` mode, so the manifest and Media Session artwork resolve.
pub(crate) const BINARY_FILES: &[(&str, &[u8], &str)] = &[
    ("/icons/icon-192.png", include_bytes!("../static/icons/icon-192.png"), "image/png"),
    ("/icons/icon-512.png", include_bytes!("../static/icons/icon-512.png"), "image/png"),
    ("/icons/maskable-512.png", include_bytes!("../static/icons/maskable-512.png"), "image/png"),
    ("/icons/apple-touch-icon.png", include_bytes!("../static/icons/apple-touch-icon.png"), "image/png"),
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
    for &(path, body, ct) in BINARY_FILES {
        router = router.route(
            path,
            get(move || async move { ([(CONTENT_TYPE, ct), (CACHE_CONTROL, "max-age=86400")], body).into_response() }),
        );
    }
    router
}
