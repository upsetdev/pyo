#!/usr/bin/env bash
# Move this machine's k3s server onto Tailscale so other machines can join the
# cluster over the tailnet. Needs root:
#
#   sudo bash scripts/tailscale-server.sh
#
# What it changes:
#   - k3s advertises its Tailscale IP and runs pod traffic (flannel) over
#     tailscale0; the API certificate gains the Tailscale IP and, with
#     MagicDNS, the machine's tailnet name
#   - ufw accepts traffic arriving over tailscale0 (k3s API, kubelet, flannel,
#     and Pyo's image registry, which listens on the Tailscale IP)
#   - the cluster join token is copied to ~/.config/pyo/k3s-node-token
#     (readable by you only) for scripts/join-node.sh
#
# k3s restarts once; running pods keep running.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "Run with sudo: sudo bash $0" >&2
  exit 1
fi
DEV_USER=${SUDO_USER:?run this through sudo so the dev user is known}
TS_IP=$(tailscale ip -4 2>/dev/null | head -n1)
if [ -z "$TS_IP" ]; then
  echo "Tailscale isn't up on this machine (tailscale ip -4 is empty)." >&2
  exit 1
fi
# Its MagicDNS name ("<host>.<tailnet>.ts.net"), when the tailnet has MagicDNS.
TS_NAME=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
TS_NAME=${TS_NAME%.}

CONFIG=/etc/rancher/k3s/config.yaml
cat > "$CONFIG" <<CONFIGEOF
# Lets the dev user read /etc/rancher/k3s/k3s.yaml without sudo.
write-kubeconfig-mode: "0644"
# Cluster traffic goes over Tailscale ($TS_IP on tailscale0).
node-ip: "$TS_IP"
advertise-address: "$TS_IP"
flannel-iface: tailscale0
tls-san:
  - "$TS_IP"
CONFIGEOF
[ -z "$TS_NAME" ] || echo "  - \"$TS_NAME\"" >> "$CONFIG"

if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow in on tailscale0 comment 'k3s over Tailscale' >/dev/null
fi

systemctl restart k3s

HOME_DIR=$(getent passwd "$DEV_USER" | cut -d: -f6)
install -d -m 700 -o "$DEV_USER" "$HOME_DIR/.config/pyo"
install -m 600 -o "$DEV_USER" /var/lib/rancher/k3s/server/node-token "$HOME_DIR/.config/pyo/k3s-node-token"

echo
echo "k3s now uses Tailscale: https://${TS_NAME:-$TS_IP}:6443"
echo "Join token: $HOME_DIR/.config/pyo/k3s-node-token"
