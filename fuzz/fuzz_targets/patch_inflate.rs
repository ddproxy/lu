#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

// Binary diffs are preflighted before libgit2 inflates them. A patch accepted under a tight cap
// must also be accepted under a looser one, and a refusal must not echo an unbounded amount of
// attacker-controlled text into an error message.
fuzz_target!(|data: &[u8]| {
    let tight = data.len() as u64;
    let loose = tight.saturating_mul(64).saturating_add(1);

    let strict = lu_staging::check_patch_inflate(data, tight);
    let relaxed = lu_staging::check_patch_inflate(data, loose);
    if strict.is_ok() {
        assert!(
            relaxed.is_ok(),
            "a patch accepted under a tight cap must be accepted under a loose cap"
        );
    }

    for result in [strict, relaxed] {
        if let Err(error) = result {
            let message = error.to_string();
            // An unknown patch header is echoed as a debug byte array, which expands roughly 6x.
            assert!(
                message.len() <= data.len().saturating_mul(8).saturating_add(256),
                "error message grew far beyond the patch: {} bytes",
                message.len()
            );
            assert!(!message.is_empty(), "a refusal must explain itself");
        }
    }
});
