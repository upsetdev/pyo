#!/usr/bin/env bash
# Works around the AppArmor bug that stops processes inside a container from
# signalling each other on Ubuntu with kernel 6.17+/7.0 (Postgres, Go
# programs and CoreDNS hang). The profile is
# crates/pyo-api/assets/cri-containerd.apparmor.d; connect.sh and join.sh
# carry a copy of it.
#
#   sudo deploy/host/apparmor-workaround.sh install   # if this host needs it
#   sudo deploy/host/apparmor-workaround.sh status
#   sudo deploy/host/apparmor-workaround.sh remove    # once k3s ships the fix
#   deploy/host/apparmor-workaround.sh needed         # exit 0: this host needs it
#   deploy/host/apparmor-workaround.sh installed      # exit 0: it's in place
set -euo pipefail

NAME=cri-containerd.apparmor.d
DEST=/etc/apparmor.d/$NAME
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/crates/pyo-api/assets/$NAME"
MARK="# Managed by Pyo"

needed() {
  # AppArmor, Ubuntu's restriction on (it stacks container labels), and a
  # kernel that stacks them: 6.17 or newer.
  command -v apparmor_parser >/dev/null || return 1
  [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_unconfined 2>/dev/null || echo 0)" = 1 ] || return 1
  local kv maj min
  kv=$(uname -r); maj=${kv%%.*}; min=${kv#*.}; min=${min%%.*}
  [ "$maj" -gt 6 ] || { [ "$maj" -eq 6 ] && [ "$min" -ge 17 ]; }
}

ours() { [ -f "$DEST" ] && head -n1 "$DEST" | grep -q "^$MARK"; }

denials() {
  journalctl -k -b --no-pager 2>/dev/null | grep 'apparmor="DENIED"' | grep 'operation="signal"' | grep -c "peer=\"$NAME//&" || true
}

case "${1:-status}" in
  install)
    [ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
    if ! needed; then
      echo "This host doesn't need it (no stacked AppArmor profiles): nothing to do."
      exit 0
    fi
    if [ -f "$DEST" ] && ! ours; then
      echo "$DEST exists and isn't Pyo's: leaving it alone." >&2
      exit 1
    fi
    install -m 0644 "$SRC" "$DEST"
    # Replacing the loaded profile applies to running containers too.
    apparmor_parser -r "$DEST"
    echo "Installed $DEST and reloaded it."
    echo "Restart stuck pods to clear hung processes, e.g.:"
    echo "  k3s kubectl -n <namespace> delete pod <pod>"
    ;;
  remove)
    [ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
    if ! ours; then
      echo "No Pyo-managed $DEST: nothing to remove."
      exit 0
    fi
    rm -f "$DEST"
    echo "Removed $DEST. Restart k3s so containerd loads its own profile again:"
    echo "  sudo systemctl restart k3s"
    ;;
  needed) needed ;;
  installed) ours ;;
  status)
    echo "restrict_unprivileged_unconfined: $(cat /proc/sys/kernel/apparmor_restrict_unprivileged_unconfined 2>/dev/null || echo n/a)"
    if ours; then echo "workaround: installed ($DEST)"; elif [ -f "$DEST" ]; then echo "workaround: $DEST exists (not Pyo's)"; else echo "workaround: not installed"; fi
    echo "signal denials this boot: $(denials)"
    ;;
  *)
    echo "Usage: $0 install|status|remove|needed|installed" >&2
    exit 2
    ;;
esac
