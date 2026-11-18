#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Arthur Jean
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Runs the daemon and the window from the working tree, together, and builds
# korictl beside them.
#
#   scripts/dev.sh             debug build
#   scripts/dev.sh --release   release build
#
# Only one korid can own the hardware: the instance lock and the per-device
# locks refuse a second one. So the installed user service is stopped for the
# length of the session and started again on the way out, whichever way the
# session ends (window closed, Ctrl+C, a build or start failure).
#
# The daemon is stopped with SIGTERM, which goes through the server's orderly
# shutdown rather than killing it mid-command.

set -euo pipefail

readonly SERVICE=korid.service
readonly READY_BANNER='listening on'
readonly READY_TIMEOUT_TENTHS=150

profile=debug
cargo_profile=()
case "${1:-}" in
  '') ;;
  --release)
    profile=release
    cargo_profile=(--release)
    ;;
  -h | --help)
    sed -n '5,17p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "dev.sh: unknown argument '$1' (try --help)" >&2
    exit 2
    ;;
esac

# korid refuses root itself; failing here saves a build that would not run.
if [[ $(id -u) -eq 0 ]]; then
  echo "dev.sh: run this as your own user, never as root" >&2
  exit 1
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
bin_dir="${CARGO_TARGET_DIR:-$root/target}/$profile"

# Built before the service is touched, so a compile error leaves the installed
# daemon running.
cargo build "${cargo_profile[@]}" -p kori-daemon --bin korid -p kori-app --bin kori \
  -p kori-cli --bin korictl

service_was_active=false
daemon_pid=
log=$(mktemp -t kori-dev.XXXXXX)

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n $daemon_pid ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill -TERM "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -f "$log"
  if $service_was_active; then
    echo "dev.sh: starting $SERVICE again"
    systemctl --user start "$SERVICE" || echo "dev.sh: could not restart $SERVICE" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if systemctl --user is-active --quiet "$SERVICE" 2>/dev/null; then
  echo "dev.sh: stopping $SERVICE for this session"
  systemctl --user stop "$SERVICE"
  service_was_active=true
fi

# The daemon's output is shown prefixed and kept in a file, so readiness is
# read from the banner it prints once the socket is bound and the devices are
# claimed. The banner names the socket the daemon actually chose, which keeps
# this script out of the path resolution in kori_core::ipc.
"$bin_dir/korid" > >(tee "$log" | sed -u 's/^/[korid] /') 2>&1 &
daemon_pid=$!

for ((i = 0; i < READY_TIMEOUT_TENTHS; i++)); do
  if grep -q "$READY_BANNER" "$log"; then
    break
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    wait "$daemon_pid" || true
    echo "dev.sh: korid exited before it was ready" >&2
    exit 1
  fi
  sleep 0.1
done
if ! grep -q "$READY_BANNER" "$log"; then
  echo "dev.sh: korid did not report ready within $((READY_TIMEOUT_TENTHS / 10)) s" >&2
  exit 1
fi

# The command-line client talks to this daemon through the same default
# socket, so an agent or a second terminal can drive the session too.
echo "dev.sh: korictl for this session is $bin_dir/korictl"

# The window runs in the foreground; closing it ends the session.
"$bin_dir/kori"
