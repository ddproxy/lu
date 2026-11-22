//! The `ting` binary: load the agent configuration, build the controller client, and run
//! until SIGINT or SIGTERM.

use std::path::Path;
use std::sync::Arc;

use lu_client::{Client, Controller};
use tuishou_ting::Agent;

pub async fn run(config_path: &Path) -> Result<(), String> {
    let config = lu_config::load(config_path).map_err(|error| error.to_string())?;
    let mut agent_config = config
        .agent
        .clone()
        .ok_or_else(|| "ting requires an [agent] config".to_owned())?;
    agent_config.workspace = config.workspace.clone();
    agent_config.stack = config.stack.clone();
    crate::logging::init(&config.log)?;

    let token = lu_config::read_private_file(
        "[agent] token_file",
        Path::new(&agent_config.token_file),
        lu_config::MAX_CONFIG_BYTES,
    )
    .map_err(|error| error.to_string())?;
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        return Err(format!("token file {} is empty", agent_config.token_file));
    }
    let ca_file = agent_config.ca_file.as_ref().map(std::path::PathBuf::from);
    let client = Client::new(&agent_config.controller_url, token, ca_file)
        .map_err(|error| error.to_string())?;
    let controller: Arc<dyn Controller> = Arc::new(client);
    let agent = Agent::new(agent_config, controller).map_err(|error| error.to_string())?;
    agent
        .run(lu_serve::shutdown_signal())
        .await
        .map_err(|error| error.to_string())
}
