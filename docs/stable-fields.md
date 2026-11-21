# Stable fields

This page is the compatibility boundary for Lu v1 data that clients read. It covers artifact file
names and the JSON field tables in [api.md](api.md). It does not cover log text, human CLI table
columns, prompts, metric names, config keys, or the SQLite schema.

> **Limit:** These are pre-1.0 releases. The changelog says semantic versioning starts at 1.0, and
> [security.md](security.md) calls support targets "not contractual guarantees". Within the
> boundary below, documented names and types are stable in this tree; additions are additive and
> announced.

## Artifacts

The controller chooses every artifact path. An agent cannot supply a name or directory.

| Item | Stable value |
| --- | --- |
| File names | Exactly `diff.patch`, `run.log`, `report.json`. |
| Path | `<artifacts_dir>/<job id>/<file name>`; symlinks are refused on reads and appends. |
| Job view | `artifacts` lists the fixed names currently stored. |
| Route | `GET /v1/jobs/{id}/artifacts/{name}` accepts one of the three names. Any other name, or a name not stored, is `404`. |
| Modes | Files `0600`, job directories `0700`; symlinks are refused rather than followed. |
| Size caps | `diff.patch` 10 MiB, `run.log` 50 MiB, `report.json` 1 MiB. |

`lu fetch` always tries those three names in that order and prints `skipped <name>: not found` for
missing ones. See [api.md](api.md#operator) and [client guide](client-guide.md#command-map).

## JSON API

The field tables in `api.md` are the v1 contract for these shapes: `JobSpec`, `JobView`, `JobEdit`,
`NodeView`, `HostStats`, `StatusView`, `RepoInfo`, `SyncResult`, `NodeStats`, `JobStats` and
`ProcessStats`.

| Rule | Promise |
| --- | --- |
| Field names and types | Stable for a documented field in these tables. |
| New fields | Additive. A response may gain a field; clients should ignore unknown response fields. |
| `JobSpec` requests | Unknown fields are ignored, so a newer client can send additive spec fields to an older controller. |
| Other request bodies | Unknown fields are rejected. Send only fields listed in `api.md`. |
| Presence rules | Omission and null rules in the tables are part of the contract: for example `outcome_note` is omitted when absent, `diff_bytes` is present only on terminal jobs, and `provider_models` is always present in `NodeView`. |
| Error envelope | `{"error":"<code>","message":"<text>"}` with an optional `length`; the code list in `api.md` is the documented set. |

Field values can still vary by state and version. An absent optional field is not an error. Do not
parse human CLI output when the JSON response is available.

## Evidence in this tree

| Change | Where it shows up |
| --- | --- |
| Unknown `JobSpec` fields ignored | `CHANGELOG.md` 0.1.14; other request bodies still reject unknown fields. |
| Additive spec and view fields | 0.2.0 fallbacks, `assigned_provider`, `assigned_model`; 0.3.0 `collect`, `include_failed`, taints, `label_sources`, `provider_models`, `diff_bytes`, `outcome_note`; 0.3.1 `on_conflict`. |
| Additive heartbeat and node fields | Changelog entries repeatedly say "No schema change" because pairs, labels, taints and `label_sources` travel in stored heartbeat data. |
| Storage schema changes | Schemas 5 and 6 added nullable columns for assigned providers and `diff_bytes`; schema 7 added `expect_failed`, schema 8 added cordon details, and schema 9 added `dispatch_seq` for persisted FIFO dispatch. Older databases upgrade on open. Clients see additive JSON fields, not a new response shape. |

## How changes are announced

- `CHANGELOG.md` at the repository root records every notable change, including additive fields and
  "No schema change" notes. It is shipped in every release archive.
- Tagged releases use generated GitHub release notes, and the release workflow uploads the archive
  and its checksum.
- Read the changelog for the exact version you deploy before depending on a newly added field.

For route details, see [api.md](api.md). For the API's fixed artifact route, see
[api.md](api.md#operator).
