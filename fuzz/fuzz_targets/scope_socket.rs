#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use lu_proto::{
    decode_socket_line, ScopeAction, ScopeErrorResponse, MAX_SCOPE_REQUEST_BYTES, SCOPE_SOURCE,
};

// The strict decoder for the inner request of a harness frame on the job socket. The agent reads one
// bounded line per connection; only a trailing CR/LF is stripped. A harness must not be able to
// smuggle a second request or oversized task through JSON framing, and every reply is tagged as
// untrusted child-job data.
fuzz_target!(|data: &[u8]| {
    match decode_socket_line(data) {
        Ok(request) => {
            let line = data
                .strip_suffix(b"\n")
                .unwrap_or(data)
                .strip_suffix(b"\r")
                .unwrap_or_else(|| data.strip_suffix(b"\n").unwrap_or(data));
            assert!(line.len() <= MAX_SCOPE_REQUEST_BYTES);
            assert!(!line.contains(&b'\n'));
            assert!(!line.contains(&b'\r'));
            assert!(ScopeAction::parse(request.action.as_str()).is_some());

            let error = ScopeErrorResponse::new("bad_request", "invalid scope request");
            assert_eq!(error.source, SCOPE_SOURCE);
            assert!(error.untrusted);
        }
        Err(error) => {
            assert!(!error.reason().is_empty());
            assert!(error.reason().len() <= 512);
        }
    }
});
