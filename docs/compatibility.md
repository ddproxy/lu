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
- The database is migrated in place and only forward: a new controller starts on the previous
  release's database. Rolling a controller back to an older release is done from the dated
  database copy, not by running the old binary on a migrated database.

## Checked in CI

`scripts/compat-test.sh <controller-bin-dir> <agent-bin-dir> [<db-seed-bin-dir>]` starts a
controller from one build and an agent from another on loopback. It confirms the node registers
and reports itself, drains and undrains, and with a third directory it starts the controller on a
database the older release created. CI runs the previous release's agent against the new controller
and the new agent against the previous release's controller, so a wire change that breaks either
fails the build.

## Updating a fleet

Because of the rule above, the order of a rollout does not matter for correctness. Updating the
controller first is still the quietest order (the controller migrates its schema once, then agents
follow), and `tuishou` updates each node independently when it is enabled.
