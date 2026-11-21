# What Lu does not protect against

Lu protects the controller boundary: bearer tokens with two scopes, SHA-256 hashes, TLS on
non-loopback listeners, epoch fencing, private file modes and capped request bodies. It does not
provide the boundaries below. Read [Security](security.md) for the controls that do exist.

| Limit | What the code does | What follows | Do this |
| --- | --- | --- | --- |
| Harness runs as the agent user | `lu agent` launches the harness as its own OS user. There is no `setuid`, namespace, container or VM switch in the launcher. | A harness can open every file that account can open: the agent token, agent config and any controller TLS key readable by that account. | Run the agent as a dedicated account. Use `harness_wrapper` to start the harness in a container or another account, with `workspace_group` for tree access. |
| Token file is readable by that user | A token file must be a regular file owned by the reader, mode `0600` or tighter, with no symlink or FIFO. The token is not passed as an argument or variable. | Mode `0600` protects against other accounts, not against the agent user or a harness running as that user. | Treat the agent account as holding the token. Do not keep unrelated secrets there. Rotate on suspicion; see [Tokens and TLS](tokens-and-tls.md#rotation). |
| Artifacts and backups carry source and secrets | `diff.patch`, `run.log` and `report.json` are the agent's report of the job. Task text, checkout contents, model output and anything a harness prints can land in them. `lu backup` copies the SQLite database; artifacts are backed up as ordinary files. | Downloads and backups are as sensitive as every repository and credential the jobs used. File modes (`0600` for files, `0700` for directories) are not redaction. | Store backups and fetch directories with the same care as the live artifact directory. See [Backup and restore](operations.md#backup-and-restore). |
| Provider keys live on the node | The controller stores provider and model names. Keys belong to the harness or a local Reiki on the agent host. `OPENCODE_SERVER_PASSWORD` is an opencode server password, not a provider-key channel. | A node, its harness or its logs can expose keys; the controller cannot audit or revoke them. | Keep provider credentials on dedicated nodes. Do not paste keys into task text. See [opencode harness](opencode-harness.md). |
| No per-job isolation | One agent runs one job at a time in a process group with run, idle and output limits. `workspace_group` gives the tree to a group; it is group sharing, not a separate user or filesystem. | Jobs on the same node share the agent account, its environment and its files. A harness can affect other jobs on that node. | Use `harness_wrapper` with a container or VM when jobs must not share a trust domain; see [Configuring labels, taints and wrappers](config.md). |
| Cordons are dispatch hints | A provider cordon blocks one `(node, provider)` pair. A job with no provider is not blocked; a job can still use another provider or another node. | A cordon is not an access control or a guarantee that a provider is safe. | Use labels and taints for placement policy, and read `lu nodes --json` before assuming a provider is usable. See [Choosing where a job runs](dispatch.md). |
| No live reload | Config, token hashes, TLS material and agent secrets are read at start. `SIGHUP` is not handled. | Changing a file has no effect until the process restarts. An agent changed while a job runs does not re-read its config mid-job. | Restart the controller or agent after changing their config or secrets; see [Operations](operations.md#upgrade). |

## Drains and cordons

They are separate mechanisms and neither is a security boundary.

- **Drain** stops a node taking any new job. Set it with `lu drain <node>`, clear it with
  `lu undrain <node>`. Both need a node that has heartbeated at least once; an unknown node is a
  `404` and the CLI exits `1`. A drain survives a controller restart.
- **Cordon** stops dispatch of jobs whose chosen provider matches the cordoned `(node, provider)`
  pair. There is no CLI command to set or clear one. `lu nodes --json` shows unexpired cordons.

A cordon clears in three ways:

1. its deadline passes (`provider_cooldown_secs`, or the agent's `retry_after_secs` clamped to
   5 seconds through 24 hours);
2. a Reiki-derived cordon sees two consecutive healthy provider heartbeats;
3. the controller restarts: boot recovery deletes every cordon row, because expiries use a
   monotonic clock that starts over with the process.

A non-Reiki cordon does not clear on healthy heartbeats. Editing the database by hand is not a
supported command. Provider failures and the operator view are described in
[Provider cordons and node drains](operations.md#provider-cordons-and-node-drains).

> Fail-open cases: an unmatched label, an untolerated taint, or no healthy pair for
> `--provider any` appears as a `waiting` reason in `lu show`. A drain or a cordon on a named
> provider can leave a job `queued` with no reason. None of them fails the job. A failing
> `[[agent.detect]]` probe removes its label; it does not stop the agent.
