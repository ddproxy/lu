#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use lu_proto::{
    decode_scope_request, ScopeAction, ScopeResponse, ScopeResult, MAX_SCOPE_REQUEST_BYTES,
    SCOPE_SOURCE,
};

// The controller's `/v1/scope/{route}` body decoder. It must reject anything oversized,
// multi-line, unknown, or carrying fields for the wrong action, and a decoded action must be one
// of the six routes that can be registered on the router.
fuzz_target!(|data: &[u8]| {
    match decode_scope_request(data) {
        Ok(request) => {
            assert!(data.len() <= MAX_SCOPE_REQUEST_BYTES);
            assert!(!data.contains(&b'\n'));
            assert!(!data.contains(&b'\r'));
            assert!(ScopeAction::parse(request.action.as_str()).is_some());

            // Re-encoding a decoded request stays bounded by the wire allowance around the task.
            let encoded = serde_json::to_vec(&request).expect("scope requests serialise");
            assert!(encoded.len() <= MAX_SCOPE_REQUEST_BYTES + 4096);
            // Replies always label their origin as untrusted child-job data.
            let response = ScopeResponse::new(ScopeResult::SubmitChild {
                job_id: request.job_id,
            });
            assert_eq!(response.source, SCOPE_SOURCE);
            assert!(response.untrusted);
        }
        Err(error) => {
            // The reason is diagnostics only; it must never be empty or echo unbounded input.
            assert!(!error.reason().is_empty());
            assert!(error.reason().len() <= 512);
        }
    }
});
