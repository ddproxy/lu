#![no_main]
#![forbid(unsafe_code)]

use axum::body::Body;
use axum::extract::Request;
use axum::http::Uri;
use libfuzzer_sys::fuzz_target;
use lu_auth::bearer_token;
use lu_controller::routes::{parse_query, read_json, read_raw_limited, MAX_JSON_BODY};
use lu_proto::{Completion, Heartbeat, LogChunk, NodeStats};

// The controller decodes agent requests from raw bytes with strict JSON types and bounded bodies.
// This target drives those public decoders directly (no HTTP server or store) and checks that
// successful decodes stay within the configured limits.
fuzz_target!(|data: &[u8]| {
    let query: String = String::from_utf8_lossy(data)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "-._%~+&=".contains(*c))
        .take(512)
        .collect();
    let uri_text = format!("/v1/agent/poll?q={query}");
    let Ok(uri) = Uri::try_from(uri_text.as_str()) else {
        return;
    };
    let make_request = || {
        Request::builder()
            .method("POST")
            .uri(uri.clone())
            .body(Body::from(data.to_vec()))
            .expect("build fuzz request")
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build fuzz runtime");
    rt.block_on(async {
        let raw = read_raw_limited(make_request(), MAX_JSON_BODY)
            .await
            .expect("raw body limit is large enough for the input");
        assert!(raw.len() <= data.len().min(MAX_JSON_BODY), "raw body grew");

        if let Ok(value) = read_json::<Completion>(make_request(), MAX_JSON_BODY).await {
            value.validate().expect("decoder must validate completions");
            let encoded = serde_json::to_string(&value).expect("serialize completion");
            assert!(
                encoded.len() <= data.len().saturating_mul(8).saturating_add(4096),
                "decoded completion grew far beyond its body"
            );
        }
        if let Ok(value) = read_json::<Heartbeat>(make_request(), MAX_JSON_BODY).await {
            let encoded = serde_json::to_string(&value).expect("serialize heartbeat");
            assert!(
                encoded.len() <= data.len().saturating_mul(8).saturating_add(16 * 1024),
                "decoded heartbeat grew far beyond its body"
            );
        }
        if let Ok(value) = read_json::<NodeStats>(make_request(), MAX_JSON_BODY).await {
            let encoded = serde_json::to_string(&value).expect("serialize stats");
            assert!(
                encoded.len() <= data.len().saturating_mul(8).saturating_add(16 * 1024),
                "decoded stats grew far beyond their body"
            );
        }
        if let Ok(value) = read_json::<LogChunk>(make_request(), MAX_JSON_BODY).await {
            value.validate().expect("decoder must validate log chunks");
            assert!(
                value.data.len() <= data.len(),
                "log chunk grew beyond its body"
            );
        }
        for (key, value) in parse_query(&uri) {
            assert!(key.len() <= uri_text.len());
            assert!(value.len() <= uri_text.len());
        }
    });

    if let Some(token) = bearer_token(&String::from_utf8_lossy(data)) {
        assert!(!token.is_empty());
        assert!(token.len() <= data.len());
        assert!(token.bytes().all(|byte| byte.is_ascii_graphic()));
    }
});
