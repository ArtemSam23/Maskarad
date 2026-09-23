//! The demo page, embedded into the binary.

use axum::response::Html;

const INDEX: &str = include_str!("../../static/index.html");

pub async fn index() -> Html<&'static str> {
    Html(INDEX)
}
