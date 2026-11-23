# Notes

- Schema version drift: `lu_store::SCHEMA_VERSION` is `5`, but several docs still describe version
  `4` as the current baseline (for example the note about `parent_job_id`/`root_job_id`/`depth` in
  [docs/api.md](docs/api.md)). The code creates new databases at version `5` and migrates versions
  `1` through `4` in place by adding columns. `docs/monitoring.md` and its `lu status` example were
  updated to `5`; the remaining prose is left for a separate docs cleanup.
- Job retention: Lu has no `DELETE FROM jobs` path. Node contribution counters are recomputed from
  the persistent `jobs` table, so they stay monotonic across controller restarts. A `since` window
  narrows the report by design and is not a monotonic counter source. If pruning is added later,
  these counters need a persistent rollup or an explicit reset semantic.
- There is no OpenAPI/Swagger document in this repository. [docs/api.md](docs/api.md) is the
  authoritative HTTP API reference; the stats route is documented there in the operator table and
  under `StatsReport`.
