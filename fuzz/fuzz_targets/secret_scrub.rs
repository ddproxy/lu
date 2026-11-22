#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use lu_client::ApiError;

// A hostile controller can echo the bearer token back in an error body. The client's redaction
// path must remove it from every message-bearing variant, whatever surrounds it.
const SECRET: &str = "lu-fuzz-token-do-not-leak";

fuzz_target!(|data: &[u8]| {
    let split = data
        .first()
        .map(|byte| usize::from(*byte) % (data.len() + 1))
        .unwrap_or(0);
    let (prefix, suffix) = data.split_at(split);
    let body = format!(
        "{}{SECRET}{}",
        String::from_utf8_lossy(prefix),
        String::from_utf8_lossy(suffix)
    );
    let status = u16::from_le_bytes([
        data.first().copied().unwrap_or(0),
        data.get(1).copied().unwrap_or(0),
    ]);

    let mut error = match data.last().copied().unwrap_or(0) % 5 {
        0 => ApiError::BadRequest {
            status,
            message: body,
        },
        1 => ApiError::Server {
            status,
            message: body,
        },
        2 => ApiError::Network { message: body },
        3 => ApiError::Decode { message: body },
        _ => ApiError::Config { message: body },
    };
    let before = error.message().len();
    error.redact_for_fuzzing(SECRET);

    let message = error.message();
    assert!(
        !message.contains(SECRET),
        "the secret survived redaction: {message}"
    );
    assert!(
        !format!("{error:?}").contains(SECRET),
        "the secret survived in Debug output"
    );
    assert!(message.len() <= before, "redaction grew the error message");
    // Lossy UTF-8 can expand each invalid byte to three bytes in the error body.
    assert!(
        message.len() <= data.len().saturating_mul(4).saturating_add(256),
        "error message grew far beyond its input"
    );
    let safe = lu_controller::error::safe_message(&message);
    assert!(
        safe.chars().count() <= 513,
        "safe message grew beyond 512 chars"
    );
    assert!(
        !safe.contains(SECRET),
        "the secret survived display sanitising"
    );
    let _ = error.status();
    let _ = error.is_transient();
});
