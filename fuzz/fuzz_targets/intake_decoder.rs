#![no_main]
#![forbid(unsafe_code)]

#[path = "intake_support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use lu_config::IntakeRule;
use lu_controller::intake::{pointer, Intake};
use serde_json::Value;
use support::{check_job, clean, config, delivery, parsed, secret_path, value, Tokens, SENTINEL};

// A signed webhook body is arbitrary JSON. Pointer resolution, sanitising and context capping
// are pure and must stay panic-free for every shape of body, pointer and template.
fuzz_target!(|data: &[u8]| {
    let mut tokens = Tokens::new(data);

    // Raw pointer lookups run even when the bytes are not JSON at all.
    let body: Value = parsed(data).unwrap_or(Value::Null);
    let probe = clean(&tokens.next());
    for path in [
        probe.as_str(),
        "",
        "/action",
        "/actor/login",
        "/nested/0",
        "/~0",
        "/~1",
    ] {
        if let Some(found) = pointer(&body, path) {
            assert_eq!(pointer(&body, path), Some(found), "pointer lookup was unstable");
            assert!(serde_json::to_vec(found).is_ok(), "pointer result must be serialisable");
        }
    }

    let Some(secret) = secret_path() else {
        return;
    };

    let repo = value(clean(&tokens.next()));
    let git_ref = value(clean(&tokens.next()));
    let task_template = clean(&tokens.next());
    let key_template = clean(&tokens.next());
    let header = clean(&tokens.next());
    let delivery_id = clean(&tokens.next());

    let mut context_from: Vec<String> = (0..8).map(|_| clean(&tokens.next())).collect();
    // Repeated pointers into one long ASCII value force the context cap and truncation branch.
    // They come last so the byte at the cut always lands inside an ASCII entry.
    context_from.extend(std::iter::repeat("/long".to_owned()).take(160));

    let rule = IntakeRule {
        matches: Vec::new(),
        allow: Vec::new(),
        repo,
        git_ref,
        base: None,
        task_template: task_template.clone(),
        task_vars: context_from.clone(),
        context_from,
        unsafe_inline: false,
        provider: None,
        model: None,
        priority: None,
        group: None,
        timeout: None,
        publish: None,
        key_template: Some(key_template),
        auto_approve: false,
        allow_forks: false,
    };

    let intake = Intake::from_config(&config(secret, vec![rule])).expect("fuzz config must compile");
    assert!(intake.has_source("fuzz"));
    assert_eq!(intake.source_names(), vec!["fuzz".to_owned()]);

    // Signature verification sees arbitrary header text and raw bytes. Only a well-formed
    // `sha256=` plus 32 hex bytes can ever be accepted.
    if intake.verify("fuzz", Some(&header), data) {
        assert!(header.starts_with("sha256="));
        assert_eq!(header.len(), 71);
    }
    let _ = intake.verify("fuzz", None, data);
    let _ = intake.verify("missing", Some(&header), data);

    let _ = intake.admit("fuzz", Some(&delivery_id));
    intake.remember("fuzz", Some(&delivery_id));
    intake.rollback("fuzz", Some(&delivery_id));

    let signed = delivery(data);
    match intake.map("fuzz", &signed) {
        Ok(Some(job)) => {
            assert_eq!(job.task, task_template, "task changed without unsafe_inline");
            assert!(!job.task.contains(SENTINEL), "payload text reached task");
            check_job(&job, "fuzz");
        }
        // A missing repo/ref pointer is an ordinary rule failure, not a fault.
        Ok(None) | Err(_) => {}
    }
});
