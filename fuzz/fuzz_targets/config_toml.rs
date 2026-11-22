#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use lu_config::{parse_http_url, Config};

// The service config is TOML supplied by an operator but still untrusted at the parser boundary.
// Validation must not panic, and both accepted configs and named errors must stay bounded.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    match Config::parse(&text) {
        Ok(config) => {
            let summary = format!("{config:?}");
            assert!(
                summary.len() <= data.len().saturating_mul(8).saturating_add(16384),
                "parsed config debug output grew far beyond its input"
            );
            assert!(
                config.controller.is_some() || config.agent.is_some(),
                "a valid config needs a controller or an agent"
            );
        }
        Err(error) => {
            let message = error.to_string();
            assert!(!message.is_empty(), "an error must name the fault");
            // toml reports unknown fields together with the fixed list of known fields, so a
            // small input can still produce a constant-size diagnostic.
            assert!(
                message.len() <= data.len().saturating_mul(4).saturating_add(4096),
                "config error grew far beyond its input"
            );
        }
    }

    // URLs are validated separately from the config; exercise the same text as a URL.
    if let Ok(parsed) = parse_http_url(text.trim()) {
        assert!(
            parsed.https || text.trim().starts_with('/'),
            "only https or absolute file URLs may be accepted"
        );
    }
});
