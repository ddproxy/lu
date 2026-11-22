# Container harness bind mount

The per-job socket is checked with `SO_PEERCRED`: the peer's numeric uid must match `[agent]
harness_uid` (or the agent's own uid when that setting is absent). Configure `harness_uid` and
`harness_gid` for a separate harness account; the agent chowns `<work_dir>/scope` and each per-job
directory/socket to that account and needs `CAP_CHOWN` (or root) to do so. A container harness needs
the socket's parent bind-mounted at the **same path** and must run as that same numeric uid.
User-namespace remapping that changes the uid seen by the agent breaks the check; with rootless
Podman, `--userns keep-id` keeps the ids aligned.

The agent sets `LU_JOB_SOCKET` in the harness environment and adds no controller credential. Forward
only that variable into the container; do not pass `LU_URL`, `LU_TOKEN_FILE`, a controller config or
an operator token.

## Helper installed on the agent host

`harness_wrapper` is an argv command, not a shell line, and Lu appends the `opencode run ...`
command as trailing arguments. A small helper resolves the per-job socket path at run time and runs
the container as the harness account (here uid/gid `1001`, replace with your harness account):

```sh
#!/bin/sh
# /usr/local/bin/lu-container-harness
set -eu
cwd=$1
shift
socket=${LU_JOB_SOCKET:?LU_JOB_SOCKET is required}
socket_dir=$(dirname "$socket")

exec podman run --rm -i \
  --userns keep-id \
  --user 1001:1001 \
  --workdir "$cwd" \
  --mount "type=bind,src=$socket_dir,dst=$socket_dir" \
  -e LU_JOB_SOCKET \
  lu-harness-image "$@"
```

The image must provide `opencode`; the appended command is executed inside the container. The helper
must itself be able to traverse the socket's `0700` parent directory, so run it as the harness
account (or from a wrapper that does). Keep the script and image under configuration management; see
the [Ansible example](scope-ansible.md).

## Agent configuration

```toml
[agent]
harness_uid = 1001
harness_gid = 1001
harness_wrapper = ["/usr/local/bin/lu-container-harness", "{cwd}"]
```

`{cwd}` is the job's working tree, also bind-mounted by the agent as for any other container
wrapper. The socket directory is `<work_dir>/scope/<job-id>-<epoch>-...`; the helper bind-mounts
that exact parent because it sits outside the job's working tree.

## Verify the boundary

Inside a running harness container, confirm the identity and the environment before trusting the
socket:

```sh
id -u                       # must print the harness account's host uid
env | grep -E '^LU_'        # must show LU_JOB_SOCKET only
test -S "$LU_JOB_SOCKET"    # a socket, not a regular file
```

A connection from a different uid fails the agent's peer-credential check before any request is
forwarded. When the job ends, loses its lease or is cancelled, the socket disappears; a retry gets a
new path in `LU_JOB_SOCKET`. See [Job-scoped capability](scope.md) for the available actions and
[Running the opencode harness](opencode-harness.md) for the other wrapper settings.
