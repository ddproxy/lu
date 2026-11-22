#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use lu_proto::{printable, truncate_chars, JobId, NodeName, JOB_ID_LEN, MAX_NAME_BYTES};

// Ids and names are parsed from routes, query strings and stored JSON. Accepted values must have
// exactly the documented shape, and the display sanitiser must never emit control characters or
// more characters than it was given.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);

    if let Ok(id) = JobId::parse(&text) {
        assert_eq!(id.as_str().len(), JOB_ID_LEN);
        assert_eq!(id.as_str(), id.as_str().to_ascii_uppercase());
        assert!(id
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()));
        assert_eq!(
            serde_json::to_string(&id).expect("serialize id").len(),
            JOB_ID_LEN + 2
        );
    }
    if let Err(error) = JobId::parse(&text) {
        let message = error.to_string();
        assert!(message.len() <= 128, "id error grew: {message}");
    }
    // Deserialisation goes through the same validator but is a separate entry point.
    let _ =
        serde_json::from_str::<JobId>(&serde_json::to_string(text.as_ref()).expect("json string"));

    if let Ok(name) = NodeName::parse(&text) {
        assert!(!name.as_str().is_empty());
        assert!(name.as_str().len() <= MAX_NAME_BYTES);
        assert!(name
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
    }
    let _ = serde_json::from_str::<NodeName>(
        &serde_json::to_string(text.as_ref()).expect("json string"),
    );

    let clean = printable(&text);
    assert_eq!(clean.chars().count(), text.chars().count());
    assert!(
        clean.chars().all(|c| !c.is_control()),
        "control character survived"
    );
    let short = truncate_chars(&clean, data.len() / 2);
    assert!(short.chars().count() <= clean.chars().count());
    assert!(clean.starts_with(&short));
});
