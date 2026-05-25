//! Integration tests for `mezame::http::mime_for`.

use mezame::http::mime_for;

#[test]
fn known_extensions() {
    assert_eq!(mime_for("index.html"), "text/html; charset=utf-8");
    assert_eq!(
        mime_for("a/b/c.js"),
        "application/javascript; charset=utf-8"
    );
    assert_eq!(
        mime_for("worker.mjs"),
        "application/javascript; charset=utf-8"
    );
    assert_eq!(mime_for("style.css"), "text/css; charset=utf-8");
    assert_eq!(mime_for("logo.png"), "image/png");
    assert_eq!(mime_for("photo.JPG"), "image/jpeg");
    assert_eq!(mime_for("favicon.ico"), "image/x-icon");
    assert_eq!(mime_for("font.woff2"), "font/woff2");
    assert_eq!(
        mime_for("manifest.webmanifest"),
        "application/manifest+json"
    );
}

#[test]
fn unknown_extension_falls_back_to_octet_stream() {
    assert_eq!(mime_for("blob.unknownext"), "application/octet-stream");
    assert_eq!(mime_for("noextension"), "application/octet-stream");
}

#[test]
fn case_insensitive_match() {
    assert_eq!(mime_for("INDEX.HTML"), "text/html; charset=utf-8");
    assert_eq!(mime_for("img.PnG"), "image/png");
}
