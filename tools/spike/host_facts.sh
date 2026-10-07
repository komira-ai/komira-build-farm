#!/usr/bin/env bash
# Host facts of a hosted runner: image, CPU, memory, disk, cgroup mode and controllers,
# pressure (PSI), user namespaces and AppArmor, ports, and the container tools present.
# Records only; nothing here can fail the job except a broken script.
. "$(dirname "$0")/lib.sh"

uid=$(id -u)
kv image "${ImageOS:-unknown} ${ImageVersion:-unknown}"
kv kernel "$(uname -r)"
kv cpu_model "$(lscpu | sed -n 's/^Model name: *//p' | head -n 1)"
kv nproc "$(nproc)"
kv mem_total_mib "$(awk '/^MemTotal/ {print int($2/1024)}' /proc/meminfo)"
kv mem_available_mib "$(awk '/^MemAvailable/ {print int($2/1024)}' /proc/meminfo)"
kv swap_total_mib "$(awk '/^SwapTotal/ {print int($2/1024)}' /proc/meminfo)"
kv disk_workspace "$(df -BG --output=size,avail,fstype "$GITHUB_WORKSPACE" | tail -n 1 | tr -s ' ')"
read -r dname dmm <<<"$(disk_of "$GITHUB_WORKSPACE")"
kv disk_workspace_device "$dname $dmm rotational=$(cat "/sys/block/$dname/queue/rotational") scheduler=$(cat "/sys/block/$dname/queue/scheduler")"
if mountpoint -q /mnt; then
    kv disk_mnt "$(df -BG --output=size,avail,fstype /mnt | tail -n 1 | tr -s ' ')"
else
    kv disk_mnt "not a mount point"
fi

kv cgroup_fs "$(stat -fc %T /sys/fs/cgroup)"
kv cgroup_root_controllers "$(cat /sys/fs/cgroup/cgroup.controllers)"
kv cgroup_root_subtree_control "$(cat /sys/fs/cgroup/cgroup.subtree_control)"
kv cgroup_self "$(self_cgroup)"
ucg=/sys/fs/cgroup/user.slice/user-$uid.slice/user@$uid.service
kv cgroup_user_service_controllers "$(cat "$ucg/cgroup.controllers" 2>/dev/null || echo 'absent (no user manager)')"
psi=""
for r in cpu memory io; do
    if [ -r "/proc/pressure/$r" ]; then psi="$psi $r"; fi
done
kv psi_files "${psi:- none}"
kv psi_cgroup_files "$(ls /sys/fs/cgroup/system.slice/*.pressure 2>/dev/null | xargs -r -n1 basename | tr '\n' ' ')"

kv userns_max "$(cat /proc/sys/user/max_user_namespaces)"
kv apparmor_restrict_unprivileged_userns "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo absent)"
kv userns_unshare_as_runner "$(try unshare --user --map-root-user true)"
kv subuid_entries "$(grep -c "^$(id -un):" /etc/subuid || true)"
kv subuid_range "$(grep "^$(id -un):" /etc/subuid | head -n 1 | cut -d: -f3)"
kv unprivileged_port_start "$(sysctl -n net.ipv4.ip_unprivileged_port_start)"
kv ephemeral_port_range "$(sysctl -n net.ipv4.ip_local_port_range | tr -s '\t ' ' ')"
kv listening_tcp_sockets "$(ss -Htln | wc -l)"
kv passwordless_sudo "$(sudo -n true 2>/dev/null && echo yes || echo no)"
kv user_runtime_dir "$([ -d "/run/user/$uid" ] && echo present || echo absent)"
kv linger "$(loginctl show-user "$uid" -p Linger 2>/dev/null || echo 'no logind user')"
kv systemd "$(systemctl --version | head -n 1)"

for b in podman crun runc conmon pasta slirp4netns fuse-overlayfs buildah docker bazel bazelisk python3 fio; do
    if command -v "$b" >/dev/null; then
        case $b in
            pasta) v=$(pasta --version 2>&1 | head -n 1) ;;
            bazel | bazelisk) v="present" ;;
            *) v=$("$b" --version 2>&1 | head -n 1) ;;
        esac
    else
        v=absent
    fi
    kv "tool_$b" "$v"
done
