# Ansible example for job-scoped access

This example prepares a node for [job-scoped access](scope.md): a private account for the harness, a
harness-owned socket root under the agent's work directory, the controller's `[scope]` switch and
limits, and a container helper. Operator and agent tokens are managed by your existing secret
handling and are deliberately not templated into the harness environment or the helper.

```yaml
---
- name: Configure lu job-scoped access
  hosts: lu_nodes
  become: true
  vars:
    lu_harness_user: lu-harness
    ting_work_dir: /var/lib/ting/work
    lu_controller_config: /etc/lu/lu-controller.toml
    ting_config: /etc/ting/ting.toml
    lu_harness_uid: 1001
    lu_harness_gid: 1001

  tasks:
    - name: Ensure the harness account exists
      ansible.builtin.user:
        name: "{{ lu_harness_user }}"
        uid: "{{ lu_harness_uid }}"
        group: "{{ lu_harness_gid }}"
        system: true
        shell: /usr/sbin/nologin
        create_home: false

    - name: Create the private socket root
      ansible.builtin.file:
        path: "{{ ting_work_dir }}/scope"
        state: directory
        owner: "{{ lu_harness_user }}"
        group: "{{ lu_harness_gid }}"
        mode: "0700"

    - name: Enable scoped actions with safe limits on the controller
      ansible.builtin.blockinfile:
        path: "{{ lu_controller_config }}"
        marker: "# {mark} lu scope"
        block: |
          [scope]
          enabled = true
          max_children_per_job = 8
          max_descendants_per_root = 32
          max_depth = 2
          max_task_bytes = 65536
          max_spawn_per_minute = 6
          max_artifact_read_bytes = 1048576
      notify: Restart lu controller

    - name: Install the container helper
      ansible.builtin.copy:
        src: files/lu-container-harness
        dest: /usr/local/bin/lu-container-harness
        owner: root
        group: root
        mode: "0755"

    - name: Point the harness wrapper at the helper
      ansible.builtin.blockinfile:
        path: "{{ ting_config }}"
        marker: "# {mark} lu scope wrapper"
        block: |
          [agent]
          work_dir = "{{ ting_work_dir }}"
          harness_uid = "{{ lu_harness_uid }}"
          harness_gid = "{{ lu_harness_gid }}"
          harness_wrapper = ["/usr/local/bin/lu-container-harness", "{cwd}"]
      notify: Restart Ting

  handlers:
    - name: Restart lu controller
      ansible.builtin.systemd:
        name: lu-controller
        state: restarted

    - name: Restart Ting
      ansible.builtin.systemd:
        name: ting
        state: restarted
```

The helper is the script in [Container harness bind mount](scope-container.md); its hard-coded
uid/gid must match the account above so `SO_PEERCRED` accepts the container's peer. The agent must
run with `CAP_CHOWN` (or as root) to give the socket root to `lu-harness`. Keep the socket root
`0700` and do not expose it to other accounts: the agent creates each per-job socket with mode
`0600` and removes it when the job ends.

`[scope] enabled = true` only opens the controller routes. A job must still be submitted with
`lu submit --allow-spawn` (and optionally `--spawn-inherit --spawn-repo <url>`) before its harness
gets a socket. The operator token remains with the operator; the harness environment gets only
`LU_JOB_SOCKET`.
