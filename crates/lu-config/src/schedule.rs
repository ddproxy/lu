//! Operator-facing scheduling configuration.
//!
//! This module validates calendar and attachment tables field by field before they are handed to
//! `lu-schedule`, so every refusal names the table and the field at fault. It is the only part of
//! `lu-config` that reads an extra file: a `[schedule] file = "/absolute/path"` reference is
//! loaded here and the resulting `Schedule` is embedded in the parsed config.

use std::collections::HashSet;
use std::path::Path;

use lu_schedule::{
    valid_calendar_name, CivilInstant, ClockTime, Days, FixedOffset, Schedule, ScheduleDefaults,
    Weekday, MAX_CALENDARS_PER_ATTACHMENT, MAX_EXCEPTIONS_PER_CALENDAR, MAX_WINDOWS_PER_CALENDAR,
};
use serde::Deserialize;

use crate::{read_config_text, validate_abs_no_dotdot, ConfigError};

const SCHEDULE_KEYS: [&str; 7] = [
    "file",
    "min_disk_free_pct",
    "horizon_secs",
    "step_budget",
    "staging_secs",
    "finish_secs",
    "starvation_secs",
];

const TUNING_KEYS: [&str; 5] = [
    "horizon_secs",
    "step_budget",
    "staging_secs",
    "finish_secs",
    "starvation_secs",
];

/// `[schedule]` — tuning values and an optional separate schedule document.
///
/// Tuning values are optional here so a referenced file can supply them without an inline value
/// silently overriding them. Defaults come from `lu-schedule` after the documents are merged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScheduleConfig {
    /// Absolute path of another TOML document with the same top-level shape.
    pub file: Option<String>,
    /// Minimum free work-filesystem space before a node may take work; zero disables the gate.
    pub min_disk_free_pct: Option<u8>,
    pub horizon_secs: Option<i64>,
    pub step_budget: Option<usize>,
    pub staging_secs: Option<i64>,
    pub finish_secs: Option<i64>,
    pub starvation_secs: Option<i64>,
}

/// The shape of a standalone `[schedule] file` document.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ScheduleDocument {
    #[serde(default)]
    schedule: Option<ScheduleConfig>,
    #[serde(default, rename = "calendar")]
    calendars: Vec<lu_schedule::Calendar>,
    #[serde(default, rename = "attach")]
    attachments: Vec<lu_schedule::Attachment>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ScheduleTuning {
    horizon_secs: Option<i64>,
    step_budget: Option<usize>,
    staging_secs: Option<i64>,
    finish_secs: Option<i64>,
    starvation_secs: Option<i64>,
}

impl ScheduleTuning {
    fn from_config(config: &ScheduleConfig) -> Self {
        Self {
            horizon_secs: config.horizon_secs,
            step_budget: config.step_budget,
            staging_secs: config.staging_secs,
            finish_secs: config.finish_secs,
            starvation_secs: config.starvation_secs,
        }
    }

    /// Inline values win over values loaded from a file; unset keys keep the earlier value.
    fn overlay(&mut self, other: Self) {
        macro_rules! overlay {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field;
                }
            };
        }
        overlay!(horizon_secs);
        overlay!(step_budget);
        overlay!(staging_secs);
        overlay!(finish_secs);
        overlay!(starvation_secs);
    }

    fn into_defaults(self) -> ScheduleDefaults {
        let base = ScheduleDefaults::default();
        ScheduleDefaults {
            horizon_secs: self.horizon_secs.unwrap_or(base.horizon_secs),
            step_budget: self.step_budget.unwrap_or(base.step_budget),
            staging_secs: self.staging_secs.unwrap_or(base.staging_secs),
            finish_secs: self.finish_secs.unwrap_or(base.finish_secs),
            starvation_secs: self.starvation_secs.unwrap_or(base.starvation_secs),
        }
    }
}

fn invalid(table: &str, reason: String) -> ConfigError {
    ConfigError::Invalid {
        table: table.to_owned(),
        reason,
    }
}

/// Translates a `lu-schedule` validation failure into a config error that names the TOML table.
pub(crate) fn map_schedule_error(error: lu_schedule::ScheduleError) -> ConfigError {
    let path = error.path();
    let reason = error.reason();
    let (table, field) = if let Some(rest) = path.strip_prefix("attach[") {
        (
            format!("[[attach]] #{}", rest.trim_end_matches(']')),
            "calendars",
        )
    } else if let Some(rest) = path.strip_prefix("calendar[") {
        let field = if reason.contains("duplicate calendar name") {
            "name"
        } else {
            "calendar"
        };
        (
            format!("[[calendar]] #{}", rest.trim_end_matches(']')),
            field,
        )
    } else if let Some(rest) = path.strip_prefix("schedule.") {
        ("[schedule]".to_owned(), rest)
    } else {
        ("[schedule]".to_owned(), path)
    };
    invalid(&table, format!("{field}: {reason}"))
}

/// Field-level validation of every schedule-related table in one TOML document.
///
/// Called before serde so bad times, offsets, days and ranges produce a `ConfigError::Invalid`
/// naming the table and the field, rather than a generic TOML parse failure.
pub(crate) fn validate_document(value: &toml::Value) -> Result<(), ConfigError> {
    let root = value
        .as_table()
        .ok_or_else(|| invalid("[config]", "configuration must be a table".to_owned()))?;
    for (former, canonical) in [("calendars", "[[calendar]]"), ("attachments", "[[attach]]")] {
        if root.contains_key(former) {
            return Err(invalid(
                "[config]",
                format!("unknown key {former:?} (use {canonical})"),
            ));
        }
    }
    let schedule = match root.get("schedule") {
        None => None,
        Some(value) => Some(
            value
                .as_table()
                .ok_or_else(|| invalid("[schedule]", "must be a table".to_owned()))?,
        ),
    };
    if let Some(table) = schedule {
        check_keys(table, &SCHEDULE_KEYS, "[schedule]")?;
        validate_tuning(table)?;
        if let Some(file) = table.get("file") {
            let path = file.as_str().ok_or_else(|| {
                invalid(
                    "[schedule]",
                    "file must be an absolute path string".to_owned(),
                )
            })?;
            validate_abs_no_dotdot("[schedule]", "file", path)?;
        }
    }

    let calendar_values = array_field(root, &["calendar"], "[config]", "calendar")?;
    let mut names = HashSet::new();
    for (index, value) in calendar_values.iter().enumerate() {
        let (name, _) = validate_calendar(value, index)?;
        let table = format!("[[calendar]] #{index} ({name})");
        if !names.insert(name.clone()) {
            return Err(invalid(&table, format!("name {name:?} is repeated")));
        }
    }

    let attachment_values = array_field(root, &["attach"], "[config]", "attach")?;
    for (index, value) in attachment_values.iter().enumerate() {
        validate_attachment(value, index, &names)?;
    }
    Ok(())
}

/// Merge inline and file-backed schedule data, validate the result, and return the pure schedule.
pub(crate) fn build(
    inline: Option<&ScheduleConfig>,
    top_calendars: &[lu_schedule::Calendar],
    top_attachments: &[lu_schedule::Attachment],
) -> Result<Option<Schedule>, ConfigError> {
    if inline.is_none() && top_calendars.is_empty() && top_attachments.is_empty() {
        return Ok(None);
    }

    let mut tuning = ScheduleTuning::default();
    let mut calendars = Vec::new();
    let mut attachments = Vec::new();

    if let Some(path) = inline.and_then(|config| config.file.as_deref()) {
        let text = read_config_text(Path::new(path))?;
        let document: ScheduleDocument = parse_document(&text, path)?;
        if let Some(file_schedule) = &document.schedule {
            if file_schedule.file.is_some() {
                return Err(invalid(
                    "[schedule] file",
                    "a schedule file must not name another file".to_owned(),
                ));
            }
            tuning = ScheduleTuning::from_config(file_schedule);
        }
        calendars.extend(document.calendars);
        attachments.extend(document.attachments);
    }

    if let Some(config) = inline {
        tuning.overlay(ScheduleTuning::from_config(config));
    }
    calendars.extend(top_calendars.iter().cloned());
    attachments.extend(top_attachments.iter().cloned());

    let schedule = Schedule {
        defaults: tuning.into_defaults(),
        calendars,
        attachments,
    };
    schedule.validate().map_err(map_schedule_error)?;
    Ok(Some(schedule))
}

fn parse_document(text: &str, path: &str) -> Result<ScheduleDocument, ConfigError> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|error| ConfigError::Parse(format!("in schedule file {path:?}: {error}")))?;
    validate_document(&value)?;
    toml::from_str(text)
        .map_err(|error| ConfigError::Parse(format!("in schedule file {path:?}: {error}")))
}

fn check_keys(table: &toml::Table, allowed: &[&str], owner: &str) -> Result<(), ConfigError> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            let hint = match key.as_str() {
                "calendars" => " (use [[calendar]])",
                "attachments" => " (use [[attach]])",
                "dates" => " (use ranges)",
                _ => "",
            };
            return Err(invalid(owner, format!("unknown key {key:?}{hint}")));
        }
    }
    Ok(())
}

fn array_field<'a>(
    table: &'a toml::Table,
    names: &[&str],
    owner: &str,
    label: &str,
) -> Result<Vec<&'a toml::Value>, ConfigError> {
    let mut found: Option<&Vec<toml::Value>> = None;
    for name in names {
        if let Some(value) = table.get(*name) {
            if found.is_some() {
                return Err(invalid(
                    owner,
                    format!("{name} must not be set together with {label}"),
                ));
            }
            let array = value.as_array().ok_or_else(|| {
                invalid(
                    owner,
                    format!("{name} must be an array of tables or strings"),
                )
            })?;
            found = Some(array);
        }
    }
    Ok(found
        .map(|array| array.iter().collect())
        .unwrap_or_default())
}

fn required_str<'a>(
    table: &'a toml::Table,
    field: &str,
    owner: &str,
) -> Result<&'a str, ConfigError> {
    table
        .get(field)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| invalid(owner, format!("{field} must be a string and is required")))
}

fn optional_str(table: &toml::Table, field: &str, owner: &str) -> Result<(), ConfigError> {
    if let Some(value) = table.get(field) {
        if value.as_str().is_none() {
            return Err(invalid(owner, format!("{field} must be a string")));
        }
    }
    Ok(())
}

fn validate_tuning(table: &toml::Table) -> Result<(), ConfigError> {
    if let Some(value) = table.get("min_disk_free_pct") {
        let number = value.as_integer().ok_or_else(|| {
            invalid(
                "[schedule]",
                "min_disk_free_pct must be an integer".to_string(),
            )
        })?;
        if !(0..=100).contains(&number) {
            return Err(invalid(
                "[schedule]",
                format!("min_disk_free_pct must be 0 to 100 (got {number})"),
            ));
        }
    }
    for field in TUNING_KEYS {
        let Some(value) = table.get(field) else {
            continue;
        };
        let number = value
            .as_integer()
            .ok_or_else(|| invalid("[schedule]", format!("{field} must be an integer")))?;
        let positive = matches!(field, "horizon_secs" | "step_budget" | "starvation_secs");
        if (positive && number <= 0) || (!positive && number < 0) {
            return Err(invalid(
                "[schedule]",
                format!(
                    "{field} must be {} (got {number})",
                    if positive { "positive" } else { "non-negative" }
                ),
            ));
        }
    }
    Ok(())
}

fn validate_calendar(
    value: &toml::Value,
    index: usize,
) -> Result<(String, FixedOffset), ConfigError> {
    let owner = format!("[[calendar]] #{index}");
    let table = value
        .as_table()
        .ok_or_else(|| invalid(&owner, "must be a table".to_owned()))?;
    check_keys(
        table,
        &[
            "name",
            "offset",
            "default",
            "windows",
            "ranges",
            "exceptions",
        ],
        &owner,
    )?;
    let name = required_str(table, "name", &owner)?.to_owned();
    let owner = format!("[[calendar]] #{index} ({name})");
    if !valid_calendar_name(&name) {
        return Err(invalid(
            &owner,
            "name must be 1 to 64 characters from A-Z a-z 0-9 . _ -".to_owned(),
        ));
    }
    let offset = match table.get("offset") {
        None => FixedOffset::UTC,
        Some(value) => {
            let text = value.as_str().ok_or_else(|| {
                invalid(
                    &owner,
                    "offset must be a string such as Z, UTC or +02:00".to_owned(),
                )
            })?;
            FixedOffset::parse(text)
                .map_err(|error| invalid(&owner, format!("offset: {}", error.reason())))?
        }
    };
    if let Some(value) = table.get("default") {
        let text = value.as_str().ok_or_else(|| {
            invalid(
                &owner,
                "default must be the string open or closed".to_owned(),
            )
        })?;
        if !matches!(text, "open" | "closed") {
            return Err(invalid(
                &owner,
                format!("default must be open or closed (got {text:?})"),
            ));
        }
    }

    let windows = array_field(table, &["windows"], &owner, "windows")?;
    if windows.len() > MAX_WINDOWS_PER_CALENDAR {
        return Err(invalid(
            &owner,
            format!("windows must name at most {MAX_WINDOWS_PER_CALENDAR} entries"),
        ));
    }
    for (window_index, window) in windows.iter().enumerate() {
        validate_window(window, &owner, window_index)?;
    }

    let ranges = array_field(table, &["ranges"], &owner, "ranges")?;
    let exceptions = array_field(table, &["exceptions"], &owner, "exceptions")?;
    if ranges.len() + exceptions.len() > MAX_EXCEPTIONS_PER_CALENDAR {
        return Err(invalid(
            &owner,
            format!(
                "ranges and exceptions must name at most {MAX_EXCEPTIONS_PER_CALENDAR} entries"
            ),
        ));
    }
    for (range_index, range) in ranges.iter().enumerate() {
        validate_range(range, &owner, "ranges", range_index, offset)?;
    }
    for (range_index, range) in exceptions.iter().enumerate() {
        validate_range(range, &owner, "exceptions", range_index, offset)?;
    }
    Ok((name, offset))
}

fn validate_window(value: &toml::Value, owner: &str, index: usize) -> Result<(), ConfigError> {
    let table_name = format!("{owner} windows[{index}]");
    let table = value
        .as_table()
        .ok_or_else(|| invalid(&table_name, "must be a table".to_owned()))?;
    check_keys(table, &["id", "kind", "from", "to", "days"], &table_name)?;
    let from_text = required_str(table, "from", &table_name)?;
    let from = ClockTime::parse(from_text)
        .map_err(|error| invalid(&table_name, format!("from: {}", error.reason())))?;
    let to_text = required_str(table, "to", &table_name)?;
    let to = ClockTime::parse(to_text)
        .map_err(|error| invalid(&table_name, format!("to: {}", error.reason())))?;
    if from == to {
        return Err(invalid(
            &table_name,
            "from and to must differ; use 24:00 as an end to close at midnight".to_owned(),
        ));
    }
    let kind = required_str(table, "kind", &table_name)?;
    if !matches!(kind, "available" | "unavailable") {
        return Err(invalid(
            &table_name,
            format!("kind must be available or unavailable (got {kind:?})"),
        ));
    }
    if let Some(days) = table.get("days") {
        validate_days(days, &table_name, "days")?;
    }
    optional_str(table, "id", &table_name)?;
    Ok(())
}

fn validate_range(
    value: &toml::Value,
    owner: &str,
    field: &str,
    index: usize,
    offset: FixedOffset,
) -> Result<(), ConfigError> {
    let table_name = format!("{owner} {field}[{index}]");
    let table = value
        .as_table()
        .ok_or_else(|| invalid(&table_name, "must be a table".to_owned()))?;
    check_keys(table, &["id", "kind", "from", "to", "reason"], &table_name)?;
    let from_text = required_str(table, "from", &table_name)?;
    let from = CivilInstant::parse(from_text, offset)
        .map_err(|error| invalid(&table_name, format!("from: {}", error.reason())))?;
    let to_text = required_str(table, "to", &table_name)?;
    let to = CivilInstant::parse(to_text, offset)
        .map_err(|error| invalid(&table_name, format!("to: {}", error.reason())))?;
    if to.to_unix() <= from.to_unix() {
        return Err(invalid(&table_name, "to must be after from".to_owned()));
    }
    if let Some(kind) = table.get("kind") {
        let kind = kind.as_str().ok_or_else(|| {
            invalid(
                &table_name,
                "kind must be the string available or unavailable".to_owned(),
            )
        })?;
        if !matches!(kind, "available" | "unavailable") {
            return Err(invalid(
                &table_name,
                format!("kind must be available or unavailable (got {kind:?})"),
            ));
        }
    }
    optional_str(table, "id", &table_name)?;
    optional_str(table, "reason", &table_name)?;
    Ok(())
}

fn validate_days(value: &toml::Value, owner: &str, field: &str) -> Result<(), ConfigError> {
    let fail = |reason: String| invalid(owner, format!("{field}: {reason}"));
    if let Some(text) = value.as_str() {
        Days::parse(text).map_err(|error| fail(error.reason().to_owned()))?;
    } else if let Some(list) = value.as_array() {
        if list.is_empty() {
            return Err(fail("must not be empty".to_owned()));
        }
        for item in list {
            let text = item
                .as_str()
                .ok_or_else(|| fail("entries must be weekday strings".to_owned()))?;
            Weekday::parse(text).map_err(|error| fail(error.reason().to_owned()))?;
        }
    } else {
        return Err(fail(
            "must be a string or an array of weekday strings".to_owned(),
        ));
    }
    Ok(())
}

fn validate_attachment(
    value: &toml::Value,
    index: usize,
    calendar_names: &HashSet<String>,
) -> Result<(), ConfigError> {
    let owner = format!("[[attach]] #{index}");
    let table = value
        .as_table()
        .ok_or_else(|| invalid(&owner, "must be a table".to_owned()))?;
    check_keys(table, &["scope", "name", "calendars"], &owner)?;
    let scope = required_str(table, "scope", &owner)?;
    if !matches!(scope, "global" | "group" | "node" | "provider" | "model") {
        return Err(invalid(
            &owner,
            format!("scope must be global, group, node, provider or model (got {scope:?})"),
        ));
    }
    let name = match table.get("name") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| invalid(&owner, "name must be a string".to_owned()))?,
        ),
    };
    if scope == "global" && name.is_some() {
        return Err(invalid(
            &owner,
            "name must not be set for a global attachment".to_owned(),
        ));
    }
    if scope != "global" && name.is_none_or(|name| name.is_empty()) {
        return Err(invalid(
            &owner,
            format!("{scope} attachment requires a name"),
        ));
    }
    if let Some(name) = name {
        if !valid_calendar_name(name) {
            return Err(invalid(
                &owner,
                "name must be 1 to 64 characters from A-Z a-z 0-9 . _ -".to_owned(),
            ));
        }
    }
    let calendars = array_field(table, &["calendars"], &owner, "calendars")?;
    if calendars.len() > MAX_CALENDARS_PER_ATTACHMENT {
        return Err(invalid(
            &owner,
            format!("calendars must name at most {MAX_CALENDARS_PER_ATTACHMENT} entries"),
        ));
    }
    for (position, value) in calendars.iter().enumerate() {
        let name = value
            .as_str()
            .ok_or_else(|| invalid(&owner, format!("calendars[{position}] must be a string")))?;
        if !calendar_names.contains(name) {
            return Err(invalid(
                &owner,
                format!("calendars[{position}] names unknown calendar {name:?}"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn base() -> String {
        format!(
            "[controller]\nlisten = \"127.0.0.1:7600\"\ndb = \"/var/lib/lu/lu.db\"\nartifacts_dir = \"/var/lib/lu/artifacts\"\n[[controller.operators]]\nname = \"ops\"\ntoken_sha256 = \"{HASH_B}\"\n[agent]\nname = \"peer-a\"\ncontroller_url = \"https://lu.example.net\"\ntoken_file = \"/etc/lu/agent.token\"\nwork_dir = \"/var/lib/lu/work\"\nharness = \"opencode\"\n[[controller.agents]]\nname = \"a\"\ntoken_sha256 = \"{HASH_A}\"\n"
        )
    }

    fn calendar_table(from: &str, to: &str, offset: &str) -> String {
        format!(
            "[[calendar]]\nname = \"night\"\noffset = \"{offset}\"\ndefault = \"closed\"\n[[calendar.windows]]\nkind = \"available\"\nfrom = \"{from}\"\nto = \"{to}\"\ndays = \"mon-fri\"\n"
        )
    }

    fn error_parts(text: &str) -> (String, String) {
        match Config::parse(text).unwrap_err() {
            ConfigError::Invalid { table, reason } => (table, reason),
            other => panic!("expected invalid config, got {other}"),
        }
    }

    #[test]
    fn a_valid_schedule_is_embedded_and_reachable() {
        let text = format!(
            "{}\n[schedule]\nhorizon_secs = 86400\nstep_budget = 1000\n{}\n[[attach]]\nscope = \"global\"\ncalendars = [\"night\"]\n",
            base(),
            calendar_table("22:00", "06:00", "+02:00")
        );
        let config = Config::parse(&text).unwrap();
        let schedule = config.schedule().expect("schedule is embedded");
        assert_eq!(schedule.defaults.horizon_secs, 86_400);
        assert_eq!(schedule.defaults.step_budget, 1_000);
        assert_eq!(schedule.calendars.len(), 1);
        assert_eq!(schedule.calendars[0].offset.as_str(), "+02:00");
        assert_eq!(schedule.attachments.len(), 1);
        assert!(Config::parse(&base()).unwrap().schedule().is_none());
    }

    #[test]
    fn former_schedule_aliases_are_refused() {
        for (text, expected) in [
            (
                format!("{}\n[[calendars]]\nname = \"office\"\n", base()),
                "use [[calendar]]",
            ),
            (
                format!("{}\n[[attachments]]\nscope = \"global\"\ncalendars = []\n", base()),
                "use [[attach]]",
            ),
            (
                format!("{}\n[schedule]\n[[schedule.calendars]]\nname = \"office\"\n", base()),
                "use [[calendar]]",
            ),
            (
                format!("{}\n[schedule]\n[[schedule.attachments]]\nscope = \"global\"\ncalendars = []\n", base()),
                "use [[attach]]",
            ),
        ] {
            let (table, reason) = error_parts(&text);
            assert!(reason.contains(expected), "{table}: {reason}");
        }
    }

    #[test]
    fn a_former_dates_field_is_refused_in_favour_of_ranges() {
        let text = format!("{}\n[[calendar]]\nname = \"office\"\ndates = []\n", base());
        let (table, reason) = error_parts(&text);
        assert!(table.contains("[[calendar]] #0"), "{table}");
        assert!(reason.contains("dates"), "{reason}");
        assert!(reason.contains("use ranges"), "{reason}");
    }

    #[test]
    fn equal_window_endpoints_are_refused_with_the_field_name() {
        let text = format!("{}\n{}", base(), calendar_table("08:00", "08:00", "Z"));
        let (table, reason) = error_parts(&text);
        assert!(table.contains("[[calendar]] #0"), "{table}");
        assert!(table.contains("windows[0]"), "{table}");
        assert!(reason.contains("from and to"), "{reason}");
    }

    #[test]
    fn an_unknown_attachment_calendar_is_refused() {
        let text = format!(
            "{}\n{}\n[[attach]]\nscope = \"global\"\ncalendars = [\"missing\"]\n",
            base(),
            calendar_table("22:00", "06:00", "Z")
        );
        let (table, reason) = error_parts(&text);
        assert_eq!(table, "[[attach]] #0");
        assert!(reason.contains("calendars"), "{reason}");
        assert!(reason.contains("missing"), "{reason}");
    }

    #[test]
    fn a_bad_calendar_offset_is_refused() {
        let text = format!("{}\n{}", base(), calendar_table("22:00", "06:00", "+15:00"));
        let (table, reason) = error_parts(&text);
        assert!(table.contains("[[calendar]] #0"), "{table}");
        assert!(reason.contains("offset"), "{reason}");
    }

    #[test]
    fn bad_days_and_unknown_keys_are_refused() {
        let text = format!(
            "{}\n[[calendar]]\nname = \"night\"\n[[calendar.windows]]\nkind = \"available\"\nfrom = \"22:00\"\nto = \"06:00\"\ndays = \"funday\"\n",
            base()
        );
        let (table, reason) = error_parts(&text);
        assert!(table.contains("windows[0]"), "{table}");
        assert!(reason.contains("days"), "{reason}");
        assert!(reason.contains("funday"), "{reason}");

        let text = format!("{}\n[schedule]\nhorizon_secs = 10\nsurprise = 1\n", base());
        let (table, reason) = error_parts(&text);
        assert_eq!(table, "[schedule]");
        assert!(reason.contains("surprise"), "{reason}");
    }

    #[test]
    fn a_schedule_file_is_loaded_and_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("schedule.toml");
        std::fs::write(
            &file,
            "[schedule]\nstep_budget = 250\n[[calendar]]\nname = \"from_file\"\n[[calendar.windows]]\nkind = \"available\"\nfrom = \"09:00\"\nto = \"17:00\"\ndays = \"mon-fri\"\n[[attach]]\nscope = \"global\"\ncalendars = [\"from_file\"]\n",
        )
        .unwrap();
        let text = format!("{}\n[schedule]\nfile = \"{}\"\n", base(), file.display());
        let config = Config::parse(&text).unwrap();
        let schedule = config.schedule().expect("file schedule is embedded");
        assert_eq!(schedule.defaults.step_budget, 250);
        assert_eq!(schedule.calendars.len(), 1);
        assert_eq!(schedule.calendars[0].name, "from_file");
        assert_eq!(schedule.attachments.len(), 1);
    }

    #[test]
    fn operator_schedule_permission_defaults_to_false() {
        let config = Config::parse(&base()).unwrap();
        let controller = config.controller.unwrap();
        assert!(!controller.operators[0].schedule);

        let text = base().replace(
            "token_sha256 = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"",
            &format!("token_sha256 = \"{HASH_B}\"\nschedule = true"),
        );
        let config = Config::parse(&text).unwrap();
        assert!(config.controller.unwrap().operators[0].schedule);
    }
}
