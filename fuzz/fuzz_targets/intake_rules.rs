#![no_main]
#![forbid(unsafe_code)]

#[path = "intake_support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;
use lu_config::IntakeRule;
use lu_controller::intake::Intake;
use support::{
    check_job, clean, condition, config, delivery, secret_path, value, Tokens, SENTINEL,
};

// Operator-written conditions and templates are configuration, but they are still fuzzed at the
// compilation boundary. With `unsafe_inline` off, payload text must never reach `task`, and no
// stage may produce control characters that were not inserted structurally.
fuzz_target!(|data: &[u8]| {
    let mut tokens = Tokens::new(data);

    let matched: Vec<_> = (0..6).map(|_| condition(&mut tokens)).collect();
    let template = clean(&tokens.next());
    let mut rule = IntakeRule {
        matches: matched,
        // Kept empty in the primary config: a failing `allow` condition drops the delivery before
        // a later rule can be tried. The generated conditions are exercised in the drop config.
        allow: Vec::new(),
        repo: value(clean(&tokens.next())),
        git_ref: value(clean(&tokens.next())),
        base: if tokens.flag() {
            Some(value(clean(&tokens.next())))
        } else {
            None
        },
        task_template: template.clone(),
        task_vars: vec![clean(&tokens.next())],
        context_from: (0..8).map(|_| clean(&tokens.next())).collect(),
        unsafe_inline: false,
        provider: None,
        model: None,
        priority: None,
        group: None,
        timeout: None,
        publish: None,
        key_template: Some(clean(&tokens.next())),
        auto_approve: tokens.flag(),
        allow_forks: tokens.flag(),
    };

    let Some(secret) = secret_path() else {
        return;
    };

    // An unconditional fallback rule keeps job building reachable when the generated conditions
    // do not match; both rules share the same template and caps.
    let mut fallback = rule.clone();
    fallback.matches.clear();
    fallback.allow.clear();
    let intake = Intake::from_config(&config(secret, vec![rule.clone(), fallback.clone()]))
        .expect("fuzz intake config must compile");

    let signed = delivery(data);
    match intake.map("fuzz", &signed) {
        Ok(Some(job)) => {
            assert_eq!(job.task, template, "task changed without unsafe_inline");
            assert!(!job.task.contains(SENTINEL), "payload text reached task");
            assert!(
                job.task.chars().all(|c| !c.is_control()),
                "task kept a control character"
            );
            check_job(&job, "fuzz");
        }
        Ok(None) | Err(_) => {}
    }

    // A second config puts generated `allow` conditions back. Most random conditions drop the
    // delivery, but the accepted ones must still satisfy every job invariant.
    let allowed: Vec<_> = (0..3).map(|_| condition(&mut tokens)).collect();
    let mut drop_rule = rule.clone();
    drop_rule.allow = allowed;
    let dropping = Intake::from_config(&config(secret, vec![drop_rule]))
        .expect("drop fuzz config must compile");
    if let Ok(Some(job)) = dropping.map("fuzz", &signed) {
        assert_eq!(job.task, template, "task changed without unsafe_inline");
        assert!(!job.task.contains(SENTINEL), "payload text reached task");
        check_job(&job, "fuzz");
    }

    // The same rules with the dangerous opt-in switched on may echo payload text, but every
    // substituted value still goes through `sanitize`.
    rule.unsafe_inline = true;
    fallback.unsafe_inline = true;
    let inline = Intake::from_config(&config(secret, vec![rule, fallback]))
        .expect("inline fuzz config must compile");
    if let Ok(Some(job)) = inline.map("fuzz", &signed) {
        assert!(
            job.task.chars().all(|c| !c.is_control()),
            "inline substitution kept a control character"
        );
        assert!(
            job.task.len() <= template.len().saturating_mul(8).saturating_add(8192),
            "inline substitution grew far beyond the template"
        );
        check_job(&job, "fuzz");
    }
});
