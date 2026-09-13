# Moved from nixlock/checks: LOCK-3 headless acceptance needs a nix-built binary plus a real
# Sway, so it runs in a nix+sway lane (future clck flake/packaging step), not in cargo CI.
# Usage stays: $0 <clck-binary> <sway> <swaymsg>

#!/usr/bin/env bash
# LOCK-3 acceptance: acquire ext-session-lock with zero outputs, add one later, and prove the
# running locker creates and configures its password surface. Every compositor IPC call uses the
# explicit socket of the throwaway PID started below; the ambient SWAYSOCK is deliberately erased.
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <nixlock> <sway> <swaymsg>" >&2
    exit 2
fi

nixlock_bin=$1
sway_bin=$2
swaymsg_bin=$3
rig=$(mktemp -d /tmp/nixlock-hotplug.XXXXXX)
runtime=$rig/runtime
config_home=$rig/config
sway_log=$rig/sway.log
locker_log=$rig/nixlock.log
sway_pid=
locker_pid=

cleanup() {
    if [[ -n $locker_pid ]] && kill -0 "$locker_pid" 2>/dev/null; then
        kill "$locker_pid"
        wait "$locker_pid" 2>/dev/null || true
    fi
    if [[ -n $sway_pid ]] && kill -0 "$sway_pid" 2>/dev/null; then
        kill "$sway_pid"
        wait "$sway_pid" 2>/dev/null || true
    fi
    case "$rig" in
        /tmp/nixlock-hotplug.*) rm -rf -- "$rig" ;;
        *) echo "refusing to remove unexpected fixture path: $rig" >&2 ;;
    esac
}
trap cleanup EXIT

mkdir -m 700 "$runtime" "$config_home"
printf '%s\n' \
    'set $mod Mod4' \
    'xwayland disable' \
    > "$rig/sway.conf"

export XDG_RUNTIME_DIR=$runtime
export XDG_CONFIG_HOME=$config_home
export WLR_BACKENDS=headless
export WLR_HEADLESS_OUTPUTS=0
export WLR_RENDERER=pixman
# The Nix Sway wrapper creates a D-Bus session only when this is absent. Sway itself needs no bus
# for this fixture; naming a private nonexistent socket avoids depending on host /etc/dbus config.
export DBUS_SESSION_BUS_ADDRESS=unix:path=$rig/nonexistent-dbus
unset SWAYSOCK

"$sway_bin" -c "$rig/sway.conf" -d >"$sway_log" 2>&1 &
sway_pid=$!
sway_socket=$runtime/sway-ipc.$(id -u).$sway_pid.sock

for _ in $(seq 1 200); do
    [[ -S $sway_socket ]] && break
    kill -0 "$sway_pid" 2>/dev/null || {
        echo "throwaway Sway exited before publishing IPC" >&2
        sed -n '1,240p' "$sway_log" >&2
        exit 1
    }
    sleep 0.05
done
[[ -S $sway_socket ]] || {
    echo "throwaway Sway did not publish its PID-bound IPC socket" >&2
    sed -n '1,240p' "$sway_log" >&2
    exit 1
}

# Prove this socket belongs to the exact private compositor before sending any IPC. The filename
# embeds the child PID, its environment names this fixture's private runtime/display, and no
# ambient SWAYSOCK is available as a fallback target.
grep -Fz "XDG_RUNTIME_DIR=$runtime" "/proc/$sway_pid/environ" >/dev/null
[[ $(stat -c %u "$sway_socket") -eq $(id -u) ]]
[[ $("$swaymsg_bin" -s "$sway_socket" -t get_outputs -r) == '[]' ]] || {
    echo "fixture did not start with zero compositor outputs" >&2
    "$swaymsg_bin" -s "$sway_socket" -t get_outputs -r >&2
    exit 1
}

wayland_socket=
for candidate in "$runtime"/wayland-*; do
    if [[ -S $candidate ]]; then
        [[ -z $wayland_socket ]] || {
            echo "throwaway Sway published more than one Wayland socket" >&2
            exit 1
        }
        wayland_socket=$candidate
    fi
done
[[ -n $wayland_socket ]] || {
    echo "throwaway Sway did not publish a Wayland display socket" >&2
    sed -n '1,240p' "$sway_log" >&2
    exit 1
}
export WAYLAND_DISPLAY=${wayland_socket##*/}

"$nixlock_bin" --debug >"$locker_log" 2>&1 &
locker_pid=$!
for _ in $(seq 1 200); do
    grep -F 'event=lock_acquired outputs=0' "$locker_log" >/dev/null && break
    kill -0 "$locker_pid" 2>/dev/null || {
        echo "nixlock exited before acquiring the zero-output lock" >&2
        sed -n '1,240p' "$locker_log" >&2
        exit 1
    }
    sleep 0.05
done
grep -F 'event=lock_acquired outputs=0' "$locker_log" >/dev/null || {
    echo "nixlock never confirmed the zero-output lock" >&2
    sed -n '1,240p' "$locker_log" >&2
    exit 1
}

"$swaymsg_bin" -s "$sway_socket" 'create_output' | grep -F '"success": true' >/dev/null
for _ in $(seq 1 200); do
    grep -F 'event=output_surface_added' "$locker_log" | grep -F 'reason=hotplug' >/dev/null &&
        grep -F 'event=surface_configured' "$locker_log" >/dev/null && break
    kill -0 "$locker_pid" 2>/dev/null || {
        echo "nixlock exited after the output hotplug" >&2
        sed -n '1,240p' "$locker_log" >&2
        exit 1
    }
    sleep 0.05
done

grep -F 'event=output_surface_added' "$locker_log" | grep -F 'role=Session' | grep -F 'reason=hotplug' >/dev/null
grep -F 'event=surface_configured' "$locker_log" | grep -F 'role=Session' >/dev/null
echo "nixlock zero-output hotplug lock surface OK"
