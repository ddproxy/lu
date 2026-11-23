use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const REQUIRED_PANELS: [&str; 6] = [
    "Utilisation per node",
    "Failure rate by class",
    "Busy share",
    "Runtime p50/p90",
    "Queue wait p50/p90",
    "Time since last finished job",
];

const REQUIRED_ALERTS: [&str; 3] = [
    "LuNodeIdleWhileJobsQueued",
    "LuNodeFailureRateHigh",
    "LuControllerMetricsMissing",
];

fn main() -> ExitCode {
    let root = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match check(&root) {
        Ok(()) => {
            println!("monitoring assets OK");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("monitoring assets invalid: {error}");
            ExitCode::FAILURE
        }
    }
}

fn yaml_key(name: &str) -> YamlValue {
    YamlValue::String(name.to_owned())
}

fn check(root: &Path) -> Result<(), String> {
    let dashboard_path = root.join("contrib/monitoring/grafana-lu-nodes.json");
    let dashboard_text = fs::read_to_string(&dashboard_path)
        .map_err(|error| format!("cannot read {}: {error}", dashboard_path.display()))?;
    let dashboard: JsonValue = serde_json::from_str(&dashboard_text)
        .map_err(|error| format!("{} is not valid JSON: {error}", dashboard_path.display()))?;
    if !dashboard.is_object() {
        return Err(format!(
            "{} must contain a JSON object",
            dashboard_path.display()
        ));
    }
    let panels = dashboard
        .get("panels")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| format!("{} has no panels array", dashboard_path.display()))?;
    let titles: Vec<&str> = panels
        .iter()
        .filter_map(|panel| panel.get("title").and_then(JsonValue::as_str))
        .collect();
    for title in REQUIRED_PANELS {
        if !titles.contains(&title) {
            return Err(format!(
                "{} is missing the {title:?} panel",
                dashboard_path.display()
            ));
        }
    }

    let alerts_path = root.join("contrib/monitoring/lu-alerts.yml");
    let alerts_text = fs::read_to_string(&alerts_path)
        .map_err(|error| format!("cannot read {}: {error}", alerts_path.display()))?;
    let alerts: YamlValue = serde_yaml::from_str(&alerts_text)
        .map_err(|error| format!("{} is not valid YAML: {error}", alerts_path.display()))?;
    let groups = alerts
        .as_mapping()
        .and_then(|mapping| mapping.get(yaml_key("groups")))
        .and_then(YamlValue::as_sequence)
        .ok_or_else(|| format!("{} has no groups sequence", alerts_path.display()))?;
    let mut names = Vec::new();
    for group in groups {
        let Some(rules) = group
            .as_mapping()
            .and_then(|mapping| mapping.get(yaml_key("rules")))
            .and_then(YamlValue::as_sequence)
        else {
            continue;
        };
        for rule in rules {
            if let Some(name) = rule
                .as_mapping()
                .and_then(|mapping| mapping.get(yaml_key("alert")))
                .and_then(YamlValue::as_str)
            {
                names.push(name);
            }
        }
    }
    for name in REQUIRED_ALERTS {
        if !names.contains(&name) {
            return Err(format!(
                "{} is missing the {name} alert",
                alerts_path.display()
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check;
    use std::path::Path;

    #[test]
    fn assets_parse() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        check(&root).expect("monitoring assets should parse");
    }
}
