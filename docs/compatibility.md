# Compatibility between releases

A fleet is updated one node at a time, so controllers and agents of different releases talk to each
other for hours or days. The rule is: **an agent may be newer than its controller, and a controller
may be newer than its agents.** Neither order needs to be forced.

## What this requires

- Agent-to-controller messages (heartbeats, polls, log chunks, completions, staged-commit and
  merge-base requests, the self-drain calls) are decoded leniently. A field the receiver does not
  know is ignored, so a newer agent can send more than an older controller expects. Values that
  are known are still validated strictly.
- Controller-to-agent and controller-to-client messages (job specs, job and node views, the status
  document) only gain optional fields with defaults, so an older agent or client reads a newer
  controller's answer.
- A new field is optional. A field is never renamed, removed or given a different meaning in a
  release; an incompatible change needs a deprecation period of at least one release in which both
  forms are accepted.
- Routes are only added. A newer agent that calls a route an older controller does not have gets a
  `404` and must treat that as "not supported", not as a failure of the node.
- Request headers and query parameters are optional. A client may send `If-Match`,
  `Idempotency-Key` or `?dry_run=1` to a controller that does not implement them; the older
  controller ignores unknown headers and query parameters and answers with its original status and
  body. A newer controller adds no required header or parameter. Dry-run and replay bodies only
  contain fields that were already allowed, and the stored `request_keys` table is the only new
  table.
- The database is migrated in place and only forward: a new controller starts on the previous
  release's database. Rolling a controller back to an older release is done from the dated
  database copy, not by running the old binary on a migrated database.

## Running the check

`scripts/compat-test.sh <controller-bin-dir> <agent-bin-dir> [<db-seed-bin-dir>]` starts a
controller from one build and an agent from another on loopback. It confirms the node registers
and reports itself, drains and undrains, and with a third directory it starts the controller on a
database the older release created. It also sends the optional `If-Match`, `Idempotency-Key` and
`?dry_run=1` controls to both builds: an older controller must ignore them and a newer one must
accept them.

With no binary directories, the script does both directions itself. It selects the latest stable
tags before the workspace version (two by default), downloads `lu-<tag>-<target>.tar.gz` and its
`.sha256` from `ddproxy/tuishou-lu` with `gh release download` or curl, compares the digest with
`sha256sum`, and extracts only a verified archive into
`target/compat-cache/<target>/<tag>/bin`. The current build comes from `target/debug`, or
`$CARGO_TARGET_DIR/debug`; the script builds it with `cargo build --locked --bins` when missing.
For each tag it runs the older agent against the new controller with a database seeded by the older
controller, then the new agent against the older controller.

| Option | Effect |
| --- | --- |
| `--previous N` | Check N preceding stable releases instead of two. |
| `--target T` | Use a release target instead of the host target. |
| `--tags-file F` | Select from a local tag list instead of querying GitHub. |
| `--dry-run` | Print the selection and planned runs without downloading or running binaries. |
| `--skip-download` | Print a message and exit 0 without running anything. |

A populated cache works offline: the archive digest and checksum file are rechecked locally on
every run, and cached binaries are reused only when their marker matches that digest. If release
lookup fails, the cached list at `target/compat-cache/<target>/tags.txt` is used. A failed download
or checksum fails the run with the asset named in the message; an unverified archive is never
extracted or executed. Skipping is only possible with an explicit `--skip-download`.

`scripts/test-compat.sh` checks version selection and checksum handling from local fixtures with
no network. `scripts/compat-test.sh --dry-run --tags-file <file>` previews a selection offline.

## Checked in CI

The `compat` job builds the current binaries and calls the script with no directories. It checks
both directions for the two latest stable releases before the workspace version, including the
in-place migration, so a wire change that breaks either direction fails the build.

## Updating a fleet

Because of the rule above, the order of a rollout does not matter for correctness. Updating the
controller first is still the quietest order (the controller migrates its schema once, then agents
follow), and `tuishou` updates each node independently when it is enabled.
