//! Validation of `[agent.harness_profiles]`, including every refusal in the config contract.

const AGENT: &str = "[agent]\nname = \"node-a\"\ncontroller_url = \"https://lu.example.net\"\ntoken_file = \"/etc/ting/ting.token\"\nwork_dir = \"/var/lib/ting/work\"\n";

fn config(profile_table: &str) -> String {
    format!("{AGENT}{profile_table}")
}

fn refuse(text: &str, fragment: &str) {
    let error = lu_config::Config::parse(text).expect_err("profile must be refused");
    let reason = match error {
        lu_config::ConfigError::Invalid { reason, .. } => reason,
        lu_config::ConfigError::Parse(reason) => reason,
        _ => panic!("unexpected error kind"),
    };
    assert!(reason.contains(fragment));
}

fn profile(name: &str, body: &str) -> String {
    config(&format!("\n[agent.harness_profiles.\"{name}\"]\n{body}"))
}

#[test]
fn a_new_profile_needs_argv_and_message() {
    refuse(&profile("fake", "message = \"go\"\n"), "argv must hold");
    refuse(
        &profile("fake", "argv = [\"/bin/true\"]\n"),
        "message must be",
    );
    lu_config::Config::parse(&profile(
        "fake",
        "argv = [\"/bin/true\"]\nmessage = \"go\"\n",
    ))
    .expect("a complete profile");
}

#[test]
fn profile_names_follow_the_job_spec_alphabet() {
    for bad in ["bad name", "bad/name", "-leading", &"a".repeat(65)] {
        refuse(
            &profile(bad, "argv = [\"/bin/true\"]\nmessage = \"go\"\n"),
            "name must",
        );
    }
    refuse(&profile("", "argv = [\"/bin/true\"]\n"), "name must");
}

#[test]
fn at_most_sixteen_profiles_including_the_builtin_are_allowed() {
    let mut body = String::new();
    for index in 0..16 {
        body.push_str(&format!(
            "\n[agent.harness_profiles.h{index}]\nargv = [\"/bin/true\"]\nmessage = \"go\"\n"
        ));
    }
    refuse(&config(&body), "at most 16 profiles");
}

#[test]
fn duplicate_profile_names_are_refused_by_the_parser() {
    let text = config(
        "\n[agent.harness_profiles.fake]\nargv = [\"/bin/true\"]\nmessage = \"go\"\n\
         [agent.harness_profiles.fake]\nargv = [\"/bin/true\"]\n",
    );
    refuse(&text, "duplicate key");
}

#[test]
fn argv_bounds_are_enforced() {
    refuse(
        &profile("fake", "argv = []\nmessage = \"go\"\n"),
        "argv must hold 1 to 64",
    );
    let many = format!(
        "argv = [{}]\nmessage = \"go\"\n",
        (0..65)
            .map(|_| "\"/bin/true\"")
            .collect::<Vec<_>>()
            .join(", ")
    );
    refuse(&profile("fake", &many), "argv must hold 1 to 64");
    let long = format!("argv = [\"{}\"]\nmessage = \"go\"\n", "x".repeat(4097));
    refuse(&profile("fake", &long), "argv entries must be 1 to 4096");
    refuse(
        &profile("fake", "argv = [\"\"]\nmessage = \"go\"\n"),
        "argv entries must be 1 to 4096",
    );
    refuse(
        &profile("fake", "argv = [\"bad\\nname\"]\nmessage = \"go\"\n"),
        "argv entries must be 1 to 4096",
    );
    let total = format!(
        "argv = [{}]\nmessage = \"go\"\n",
        (0..5)
            .map(|_| format!("\"{}\"", "x".repeat(4096)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    refuse(
        &profile("fake", &total),
        "argv must hold at most 16384 bytes",
    );
}

#[test]
fn message_rules_are_enforced() {
    refuse(
        &profile("fake", "argv = [\"/bin/true\"]\nmessage = \"\"\n"),
        "message must be 1 to 4096",
    );
    let long = format!(
        "argv = [\"/bin/true\"]\nmessage = \"{}\"\n",
        "x".repeat(4097)
    );
    refuse(&profile("fake", &long), "message must be 1 to 4096");
    refuse(
        &profile("fake", "argv = [\"/bin/true\"]\nmessage = \"a\\tb\"\n"),
        "message must be 1 to 4096",
    );
    refuse(
        &profile(
            "fake",
            "argv = [\"/bin/true\"]\nmessage = \"see {model}\"\n",
        ),
        "unknown placeholder {model}",
    );
    lu_config::Config::parse(&profile(
        "fake",
        "argv = [\"/bin/true\", \"{message}\"]\nmessage = \"go {task_file}{context_warning}\"\n",
    ))
    .expect("inline message placeholders are allowed");
}

#[test]
fn fragment_arrays_are_bounded_and_wrapper_only_accepts_cwd() {
    let many = format!(
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nwrapper = [{}]\n",
        (0..17)
            .map(|_| "\"/bin/sh\"")
            .collect::<Vec<_>>()
            .join(", ")
    );
    refuse(&profile("fake", &many), "wrapper must hold at most 16");
    refuse(
        &profile(
            "fake",
            "argv = [\"/bin/true\"]\nmessage = \"go\"\nwrapper = [\"{model}\"]\n",
        ),
        "unknown placeholder {model}",
    );
    for field in ["model_args", "provider_args", "server_args"] {
        let many = format!(
            "argv = [\"/bin/true\"]\nmessage = \"go\"\n{field} = [{}]\n",
            (0..17).map(|_| "\"x\"").collect::<Vec<_>>().join(", ")
        );
        refuse(
            &profile("fake", &many),
            &format!("{field} must hold at most 16"),
        );
        refuse(
            &profile(
                "fake",
                &format!("argv = [\"/bin/true\"]\nmessage = \"go\"\n{field} = [\"\"]\n"),
            ),
            "entries must be 1 to 4096",
        );
    }
    lu_config::Config::parse(&profile(
        "fake",
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nwrapper = [\"runner\", \"--workdir\", \"{cwd}\"]\n",
    ))
    .expect("cwd is allowed in the wrapper");
}

#[test]
fn placeholders_are_a_closed_vocabulary() {
    refuse(
        &profile("fake", "argv = [\"{bogus}\"]\nmessage = \"go\"\n"),
        "unknown placeholder {bogus}",
    );
    refuse(
        &profile("fake", "argv = [\"prefix{}\"]\nmessage = \"go\"\n"),
        "malformed placeholder",
    );
    refuse(
        &profile("fake", "argv = [\"open{\"]\nmessage = \"go\"\n"),
        "unmatched '{'",
    );
    refuse(
        &profile("fake", "argv = [\"close}\"]\nmessage = \"go\"\n"),
        "unmatched '}'",
    );
    refuse(
        &profile(
            "fake",
            "argv = [\"pre{model_args}post\"]\nmessage = \"go\"\n",
        ),
        "must be the whole argv element",
    );
    lu_config::Config::parse(&profile(
        "fake",
        "argv = [\"{cwd}\", \"{task_file}\", \"{message}\", \"{job_socket}\", \"{result_file}\", \"{server}\", \"{provider}\", \"{model}\", \"{timeout_secs}\", \"{model_args}\", \"{provider_args}\", \"{server_args}\"]\nmessage = \"go\"\n",
    ))
    .expect("all documented placeholders are accepted");
}

#[test]
fn env_names_are_a_whitelist_of_names() {
    let many = format!(
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nenv = [{}]\n",
        (0..33)
            .map(|index| format!("\"V{index}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    refuse(&profile("fake", &many), "env must name at most 32");
    for bad in ["1BAD", "bad-name"] {
        refuse(
            &profile(
                "fake",
                &format!("argv = [\"/bin/true\"]\nmessage = \"go\"\nenv = [\"{bad}\"]\n"),
            ),
            "env names must match",
        );
    }
    for bad in ["LU_EXTRA", "LU_TOKEN_FILE"] {
        refuse(
            &profile(
                "fake",
                &format!("argv = [\"/bin/true\"]\nmessage = \"go\"\nenv = [\"{bad}\"]\n"),
            ),
            "env may not name a controller credential or an LU_ variable",
        );
    }
    lu_config::Config::parse(&profile(
        "fake",
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nenv = [\"GOOD_NAME1\"]\n",
    ))
    .expect("a plain environment name");
}

#[test]
fn secret_env_names_and_paths_are_bounded() {
    let entries = (0..9)
        .map(|index| format!("S{index} = \"/private/path\""))
        .collect::<Vec<_>>()
        .join(", ");
    refuse(
        &profile(
            "fake",
            &format!("argv = [\"/bin/true\"]\nmessage = \"go\"\nsecret_env = {{ {entries} }}\n"),
        ),
        "secret_env must name at most 8",
    );
    refuse(
        &profile("fake", "argv = [\"/bin/true\"]\nmessage = \"go\"\nsecret_env = { LU_KEY = \"/private/path\" }\n"),
        "secret_env may not name a controller credential or an LU_ variable",
    );
    refuse(
        &profile(
            "fake",
            "argv = [\"/bin/true\"]\nmessage = \"go\"\nsecret_env = { KEY = \"\" }\n",
        ),
        "must name a non-empty path",
    );
}

#[test]
fn stdin_is_exactly_null_or_task() {
    refuse(
        &profile(
            "fake",
            "argv = [\"/bin/true\"]\nmessage = \"go\"\nstdin = \"file\"\n",
        ),
        "unknown variant",
    );
    lu_config::Config::parse(&profile(
        "fake",
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nstdin = \"task\"\n",
    ))
    .expect("task stdin");
}

#[test]
fn result_file_stays_relative_and_literal() {
    for bad in [
        "\"/tmp/result\"",
        "\"../result\"",
        "\"a/../b\"",
        "\"bad\\nname\"",
    ] {
        refuse(
            &profile(
                "fake",
                &format!("argv = [\"/bin/true\"]\nmessage = \"go\"\nresult_file = {bad}\n"),
            ),
            "result_file must be a relative path",
        );
    }
    let long = format!(
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nresult_file = \"{}\"\n",
        "x".repeat(4097)
    );
    refuse(
        &profile("fake", &long),
        "result_file must be a relative path",
    );
}

#[test]
fn marker_arrays_are_bounded() {
    let many = format!(
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nprovider_markers = [{}]\n",
        (0..17).map(|_| "\"x\"").collect::<Vec<_>>().join(", ")
    );
    refuse(
        &profile("fake", &many),
        "provider_markers must hold at most 16",
    );
    refuse(
        &profile(
            "fake",
            "argv = [\"/bin/true\"]\nmessage = \"go\"\nneeds_input_markers = [\"\"]\n",
        ),
        "needs_input_markers entries must be 1 to 64",
    );
    let long = format!(
        "argv = [\"/bin/true\"]\nmessage = \"go\"\nneeds_input_markers = [\"{}\"]\n",
        "x".repeat(65)
    );
    refuse(
        &profile("fake", &long),
        "needs_input_markers entries must be 1 to 64",
    );
    refuse(
        &profile(
            "fake",
            "argv = [\"/bin/true\"]\nmessage = \"go\"\nprovider_markers = [\"a\\tb\"]\n",
        ),
        "provider_markers entries must be 1 to 64",
    );
}

#[test]
fn server_values_keep_the_former_launcher_checks() {
    for bad in [
        "ftp://example.net",
        "https://example .net",
        "https://exa\\nmple.net",
        "-https://example.net",
        "",
    ] {
        refuse(
            &config(&format!(
                "\n[agent.harness_profiles.opencode]\nserver = \"{bad}\"\n"
            )),
            "server must be an http:// or https:// URL",
        );
    }
    let long = format!(
        "\n[agent.harness_profiles.opencode]\nserver = \"https://{}\"\n",
        "x".repeat(2048)
    );
    refuse(&config(&long), "server must be an http:// or https:// URL");
}

#[test]
fn builtin_overrides_are_validated_after_merge() {
    refuse(
        &config("\n[agent.harness_profiles.opencode]\nargv = []\n"),
        "argv must hold 1 to 64",
    );
    refuse(
        &config("\n[agent.harness_profiles.opencode]\nunknown = true\n"),
        "unknown field",
    );
}

#[test]
fn former_agent_keys_are_unknown() {
    for key in [
        "harness",
        "opencode_server",
        "opencode_password_file",
        "harness_wrapper",
    ] {
        // `harness` is the one-release parse-only alias; the other three are gone.
        if key == "harness" {
            continue;
        }
        refuse(&format!("{AGENT}{key} = \"gone\"\n"), "unknown field");
    }
}
