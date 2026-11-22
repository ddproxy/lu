#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use lu_proto::{
    JobEdit, JobSpec, MAX_AFTER_JOBS, MAX_FALLBACKS, MAX_REQUIRES_LABELS, MAX_TAINTS,
    MAX_TASK_BYTES,
};

// Job specs arrive as agent/operator JSON. Deserialisation already validates the fields; the
// harness checks that accepted specs stay within the documented caps and that canonical output
// cannot grow without bound relative to the input.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let spec = serde_json::from_str::<JobSpec>(text.as_ref())
        .or_else(|_| serde_json::from_slice::<JobSpec>(data));

    if let Ok(spec) = spec {
        spec.validate().expect("deserialised spec must validate");
        let canonical = spec.canonical_json();
        assert!(
            canonical.len() <= data.len().saturating_mul(8).saturating_add(8192),
            "canonical spec grew far beyond its input"
        );
        // Fields come from the lossy UTF-8 view of the input, so compare against that length.
        assert!(spec.task.len() <= text.len());
        assert!(spec.task.len() <= MAX_TASK_BYTES);
        assert!(spec.repo.len() <= text.len());
        assert!(spec.after.len() <= MAX_AFTER_JOBS);
        assert!(spec.requires.len() <= MAX_REQUIRES_LABELS);
        assert!(spec.tolerates.len() <= MAX_TAINTS);
        assert!(spec.fallbacks.len() <= MAX_FALLBACKS);
        assert!(spec.git_ref.len() <= text.len());

        if let Ok(edit) = serde_json::from_str::<JobEdit>(text.as_ref()) {
            match edit.apply(&spec) {
                Ok(edited) => edited
                    .validate()
                    .expect("an accepted edit must leave a valid spec"),
                Err(error) => {
                    let message = error.to_string();
                    assert!(
                        message.len() <= data.len().saturating_add(256),
                        "unbounded edit error"
                    );
                }
            }
        }
    }

    if let Err(error) = serde_json::from_str::<JobSpec>(text.as_ref()) {
        let message = error.to_string();
        assert!(
            message.len() <= data.len().saturating_mul(4).saturating_add(256),
            "unbounded spec parse error"
        );
    }
});
