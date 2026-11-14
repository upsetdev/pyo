#!/usr/bin/env bash
# Makes this machine a Pyo cluster: k3s (if it isn't installed) and the
# Pyo agent. Pyo's "Add cluster" gives you the full command, e.g.:
#
#   curl -fsSL https://your-pyo/install/connect.sh | bash -s -- \
#     --api http://pyo.tail1234.ts.net:8000 --token 3f9a... [--tailscale-key tskey-auth-...] \
#     [--registry pyo.tail1234.ts.net:5005 --registry-token 9b1c...]
#
#   --api            where the agent reaches Pyo
#   --token          the cluster's agent token (shown once in Pyo)
#   --tailscale-key  joins your tailnet first, when Pyo is reachable over it
#   --registry       Pyo's registry door, for images Pyo builds
#   --registry-token this cluster's login there (pull only)
#
# Or removes what Pyo put into this cluster, leaving the cluster itself
# (k3s stays):
#
#   curl -fsSL https://your-pyo/install/connect.sh | bash -s -- --uninstall
#
#   --keep-data      keep project namespaces, databases and their volumes;
#                    the volumes of what's removed keep their data on disk
#   --yes            don't ask first
#   --dry-run        only say what it would delete
# With KUBECONFIG set, it uses kubectl and that cluster instead of k3s's.
# >>> pyo:become-root (the same in connect.sh and join.sh)
# Root, through sudo when it isn't: the script runs again as root, with
# the same arguments and Tailscale key. Run from a file, sudo runs that
# file. Piped into bash (curl ... | bash), the new shell reads the rest of
# the script from the same pipe, where bash left it: just after the line
# that calls this. So it's called before anything else runs, and before
# `set -euo pipefail`, which the rest sets again.
become_root() {
  [ "$(id -u)" = 0 ] && return 0
  if ! command -v sudo >/dev/null; then
    echo "Pyo needs root, and there's no sudo here. Run this as root." >&2
    exit 1
  fi
  local self=${BASH_SOURCE[1]:-}
  echo "Pyo needs root. Asking sudo..." >&2
  if [ -n "$self" ] && [ -f "$self" ]; then
    exec sudo env TS_AUTHKEY="${TS_AUTHKEY:-}" bash "$self" "$@"
  elif [ ! -t 0 ]; then
    exec sudo env TS_AUTHKEY="${TS_AUTHKEY:-}" bash -s -- "$@"
  fi
  echo "Run this as root (sudo)." >&2
  exit 1
}
# <<< pyo:become-root
# --uninstall uses kubectl as you with KUBECONFIG set, or without k3s here.
case " $* " in *" --uninstall "*) [ -n "${KUBECONFIG:-}" ] || ! command -v k3s >/dev/null || become_root "$@" ;; *) become_root "$@" ;; esac
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

API="" TOKEN="" TS_AUTHKEY="${TS_AUTHKEY:-}" REGISTRY="" REGISTRY_TOKEN=""
UNINSTALL="" KEEP_DATA="" YES="" DRY_RUN="" WITH_PYO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --api) API="${2%/}"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --tailscale-key) TS_AUTHKEY="$2"; shift 2 ;;
    --registry) REGISTRY="$2"; shift 2 ;;
    --registry-token) REGISTRY_TOKEN="$2"; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    --keep-data) KEEP_DATA=1; shift ;;
    --yes | -y) YES=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    # install.sh --uninstall on a k3s Pyo didn't install: Pyo itself goes too.
    --with-pyo) WITH_PYO=1; shift ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done

# ── connect.sh --uninstall ──────────────────────────────────────────────────
# Deletes only what Pyo made, found by its names and labels: the agent
# first (so nothing is put back), then projects and databases (while their
# operators still run, for their finalizers), then the system bundles, Pyo
# Gateway, cert-manager (if Pyo installed it and nothing else uses it), and
# the agent's roles. Other namespaces, CRDs and Gateway API stay. With
# --keep-data, the volumes in the namespaces it removes (Pyo's own database,
# registry and backup store, with --with-pyo) are set to Retain first: their
# data and PersistentVolumes stay, listed at the end.
uninstall() {
  local KC
  if [ -z "${KUBECONFIG:-}" ] && command -v k3s >/dev/null; then
    [ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
    KC=(k3s kubectl)
  elif command -v kubectl >/dev/null; then
    KC=(kubectl)
  else
    echo "There's no k3s or kubectl on this machine. Run this on the cluster's first server." >&2
    exit 1
  fi
  kc() { "${KC[@]}" "$@"; }
  # Changes go through here: --dry-run only prints them.
  act() {
    if [ -n "$DRY_RUN" ]; then echo "  would run: kubectl $*" >&4; else kc "$@"; fi
  }
  lines() { kc get "$@" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null || true; }
  has() { kc get "$@" >/dev/null 2>&1; }
  ours() { [ "$(kc get "$@" -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)" = pyo ]; }
  # Waits until `kc get <args>` lists nothing (up to $1 seconds).
  gone() {
    local secs=$1 i
    shift
    [ -z "$DRY_RUN" ] || return 0
    for ((i = 0; i < secs; i += 3)); do
      [ -z "$(kc get "$@" -o name 2>/dev/null || true)" ] && return 0
      sleep 3
    done
    return 1
  }
  del_ns() { # namespaces, waited for
    local ns
    for ns in "$@"; do act delete namespace "$ns" --ignore-not-found --wait=false >/dev/null; done
    for ns in "$@"; do
      gone 600 namespace "$ns" --ignore-not-found || {
        echo "Namespace $ns is still being deleted after 10 minutes. Look at: kubectl get all -n $ns" >&2
        echo "Run this again once it's gone. Nothing after it was removed yet." >&2
        exit 1
      }
    done
  }

  exec 4>&1
  has namespace kube-system || { echo "Can't reach the cluster with ${KC[*]}." >&2; exit 1; }
  local server
  server=$(kc config view --minify -o jsonpath='{.clusters[0].cluster.server}' 2>/dev/null || true)

  local pyo_here=""
  has -n pyo-system deployment pyo && pyo_here=1
  if [ -n "$pyo_here" ] && [ -z "$WITH_PYO" ]; then
    echo "This cluster runs Pyo itself. To remove Pyo from this machine, run:" >&2
    echo "  curl -fsSL https://pyo.dev/install.sh | bash -s -- --uninstall" >&2
    exit 1
  fi

  # What's here.
  local projects=() bundles=() ns id line
  mapfile -t projects < <(lines namespace -l pyo.dev/managed=true | grep -vx pyo-system || true)
  while IFS= read -r line; do
    id=${line%% *} ns=${line#* }
    [ -n "$id" ] && [ -n "$ns" ] || continue
    case "$ns" in default | kube-* | pyo-system | pyo-gateway | cert-manager) continue ;; esac
    bundles+=("$id=$ns")
  done < <(kc get namespace -l pyo.dev/bundle -o jsonpath='{range .items[*]}{.metadata.labels.pyo\.dev/bundle} {.metadata.name}{"\n"}{end}' 2>/dev/null || true)
  local dbs=0
  has crd databases.pyo.dev && dbs=$(kc get databases.pyo.dev -A --no-headers 2>/dev/null | wc -l)
  local keep_db_system=""
  if [ -n "$KEEP_DATA" ]; then
    for line in "${bundles[@]}"; do [ "${line%%=*}" = databases ] && keep_db_system=1; done
  fi
  local gateway_chart="" cm_chart=""
  ours -n kube-system helmchart.helm.cattle.io envoy-gateway && gateway_chart=1
  ours -n kube-system helmchart.helm.cattle.io cert-manager && cm_chart=1
  # cert-manager stays when something else uses it: certificates or issuers
  # outside the namespaces this removes.
  local cm_users="" removed_ns=" pyo-system pyo-gateway cert-manager "
  for line in "${bundles[@]}"; do
    [ -n "$keep_db_system" ] && [ "${line%%=*}" = databases ] && continue
    removed_ns+="${line#*=} "
  done
  [ -n "$KEEP_DATA" ] || removed_ns+="${projects[*]} "
  if [ -n "$cm_chart" ]; then
    cm_users=$(kc get certificates.cert-manager.io,issuers.cert-manager.io -A -o jsonpath='{range .items[*]}{.metadata.namespace}{"\n"}{end}' 2>/dev/null | sort -u | while read -r ns; do
      case "$removed_ns" in *" $ns "*) ;; *) echo "$ns" ;; esac
    done | xargs)
  fi
  local remove_cm=""
  [ -n "$cm_chart" ] && [ -z "$keep_db_system" ] && [ -z "$cm_users" ] && remove_cm=1
  # Keeping data: the volumes claimed in namespaces this removes, kept on
  # disk (Retain) as "name|namespace/claim|policy|path|server".
  local retain=() pv claim_ns claim policy path node
  if [ -n "$KEEP_DATA" ]; then
    while IFS='|' read -r pv claim_ns claim policy path node; do
      [ -n "$pv" ] && [ -n "$claim_ns" ] || continue
      case "$removed_ns" in *" $claim_ns "*) ;; *) continue ;; esac
      retain+=("$pv|$claim_ns/$claim|$policy|$path|$node")
    done < <(kc get pv -o jsonpath='{range .items[*]}{.metadata.name}|{.spec.claimRef.namespace}|{.spec.claimRef.name}|{.spec.persistentVolumeReclaimPolicy}|{.spec.local.path}{.spec.hostPath.path}|{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]}{"\n"}{end}' 2>/dev/null || true)
  fi

  echo "This removes Pyo from the cluster at ${server:-?}:"
  echo "  - the Pyo agent, its roles and admission policy (namespace pyo-system)"
  [ -z "$pyo_here" ] || echo "  - Pyo itself, with its own database, registry and backup store (pyo-system)"
  has namespace pyo-gateway && echo "  - Pyo Gateway (namespace pyo-gateway${gateway_chart:+, Envoy Gateway})"
  for line in "${bundles[@]}"; do
    [ -n "$keep_db_system" ] && [ "${line%%=*}" = databases ] && continue
    echo "  - the ${line%%=*} system (namespace ${line#*=})"
  done
  [ -z "$remove_cm" ] || echo "  - cert-manager (namespace cert-manager), which Pyo installed"
  if [ -z "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    echo "  - ALL PROJECT DATA: ${#projects[@]} namespace(s) with their services, volumes and $dbs database(s):"
    echo "      ${projects[*]}"
  fi
  local kept=()
  if [ -n "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    kept+=("project namespaces, with their services, volumes and $dbs database(s): ${projects[*]}")
  fi
  if [ -n "$keep_db_system" ]; then kept+=("the databases system that runs them (pyo-databases), and cert-manager"); fi
  if [ -n "$cm_users" ] && [ -z "$keep_db_system" ]; then kept+=("cert-manager, which these namespaces use: $cm_users"); fi
  if [ "${#kept[@]}" -gt 0 ]; then
    echo "Kept:"
    printf '  - %s\n' "${kept[@]}"
  fi
  if [ "${#retain[@]}" -gt 0 ]; then
    echo "Kept on the servers' disks, with their volumes (set to Retain), the data of:"
    for line in "${retain[@]}"; do
      IFS='|' read -r pv claim policy path node <<<"$line"
      echo "  - $claim: PersistentVolume $pv${path:+, $path}${node:+ on $node}"
    done
  fi
  if [ -z "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    echo
    echo "Deleted data can't be brought back. Back it up first: https://pyo.dev/docs/uninstall"
  fi
  echo
  if [ -n "$DRY_RUN" ]; then
    echo "Dry run: nothing is changed."
  elif [ -z "$YES" ]; then
    local answer=""
    if [ -r /dev/tty ] && { exec 3</dev/tty; } 2>/dev/null; then
      read -r -p "Type yes to remove it: " answer <&3 || true
      exec 3<&-
    else
      echo "Run again with --yes to remove it without asking." >&2
      exit 1
    fi
    [ "$answer" = yes ] || { echo "Nothing was changed."; exit 1; }
  fi

  # 0. Kept volumes: Retain, so deleting their claims keeps their data.
  for line in "${retain[@]}"; do
    IFS='|' read -r pv claim policy path node <<<"$line"
    [ "$policy" = Retain ] || act patch pv "$pv" --type=merge -p '{"spec":{"persistentVolumeReclaimPolicy":"Retain"}}' >/dev/null
  done

  # 1. The agent, so it stops putting things back; its admission policy.
  echo "Stopping the Pyo agent…"
  act -n pyo-system delete deployment pyo-agent --ignore-not-found --wait=true --timeout=120s >/dev/null
  act delete validatingadmissionpolicybinding,validatingadmissionpolicy -l app.kubernetes.io/part-of=pyo-agent --ignore-not-found >/dev/null 2>&1 || true

  # 2. Projects and databases, while the operators run for their finalizers;
  # or, keeping them, only the agent's bindings in them.
  if [ -z "$KEEP_DATA" ]; then
    if [ "$dbs" -gt 0 ]; then
      echo "Deleting $dbs database(s)…"
      act delete databases.pyo.dev --all -A --wait=false >/dev/null
      gone 600 databases.pyo.dev -A || echo "Some databases are still being deleted. Carrying on with their namespaces." >&2
    fi
    if [ "${#projects[@]}" -gt 0 ]; then
      echo "Deleting ${#projects[@]} project namespace(s)…"
      del_ns "${projects[@]}"
    fi
  else
    for ns in "${projects[@]}"; do
      if [ "$(kc -n "$ns" get rolebinding pyo-agent -o jsonpath='{.roleRef.name}' 2>/dev/null || true)" = pyo-agent-write ]; then
        act -n "$ns" delete rolebinding pyo-agent --ignore-not-found >/dev/null
      fi
    done
  fi

  # 3. System bundles: their webhooks, bindings, namespace, then CRDs and
  # cluster roles (all labelled pyo.dev/bundle=<id>).
  for line in "${bundles[@]}"; do
    id=${line%%=*} ns=${line#*=}
    [ -n "$keep_db_system" ] && [ "$id" = databases ] && continue
    echo "Removing the $id system ($ns)…"
    act delete validatingwebhookconfiguration,mutatingwebhookconfiguration -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
    act delete validatingadmissionpolicybinding,validatingadmissionpolicy -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null 2>&1 || true
    act delete rolebinding -A -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
    del_ns "$ns"
    act delete crd -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
    act delete clusterrolebinding,clusterrole -l "pyo.dev/bundle=$id" --ignore-not-found >/dev/null
  done

  # 4. Pyo Gateway: its routes and Gateways (Envoy Gateway tears down their
  # proxies), its class, then Envoy Gateway itself and its CRDs.
  if has namespace pyo-gateway; then
    echo "Removing Pyo Gateway…"
    has crd gateways.gateway.networking.k8s.io && act -n pyo-gateway delete gateways.gateway.networking.k8s.io,httproutes.gateway.networking.k8s.io --all --ignore-not-found --wait=false >/dev/null
    has crd tcproutes.gateway.networking.k8s.io && act -n pyo-gateway delete tcproutes.gateway.networking.k8s.io --all --ignore-not-found --wait=false >/dev/null
    has crd udproutes.gateway.networking.k8s.io && act -n pyo-gateway delete udproutes.gateway.networking.k8s.io --all --ignore-not-found --wait=false >/dev/null
    has crd gateways.gateway.networking.k8s.io && { gone 120 -n pyo-gateway gateways.gateway.networking.k8s.io || true; }
  fi
  if ours gatewayclass.gateway.networking.k8s.io pyo; then
    act delete gatewayclass.gateway.networking.k8s.io pyo --ignore-not-found --wait=false >/dev/null
    gone 60 gatewayclass.gateway.networking.k8s.io pyo --ignore-not-found || act patch gatewayclass.gateway.networking.k8s.io pyo --type=merge -p '{"metadata":{"finalizers":null}}' >/dev/null 2>&1 || true
  fi
  if [ -n "$gateway_chart" ]; then
    act -n kube-system delete helmchart.helm.cattle.io envoy-gateway --ignore-not-found --wait=false >/dev/null
    gone 300 -n kube-system helmchart.helm.cattle.io envoy-gateway --ignore-not-found || echo "Envoy Gateway's chart is still being removed. Carrying on." >&2
  fi
  has namespace pyo-gateway && del_ns pyo-gateway
  # Its CRDs (applied before its chart), unless another Envoy Gateway runs
  # here. The Gateway API stays, k3s's or the copy Pyo applied.
  if [ -n "$gateway_chart" ] && [ -z "$(kc get deployment -A -l control-plane=envoy-gateway -o jsonpath='{range .items[*]}{.metadata.namespace}{"\n"}{end}' 2>/dev/null | grep -vx pyo-gateway || true)" ]; then
    local crds=()
    mapfile -t crds < <(lines crd | grep '\.gateway\.envoyproxy\.io$' || true)
    [ "${#crds[@]}" -eq 0 ] || act delete crd "${crds[@]}" --ignore-not-found >/dev/null
  fi

  # 5. cert-manager: Pyo's issuers; cert-manager itself if Pyo installed it
  # and nothing else uses it.
  act delete clusterissuer.cert-manager.io -l app.kubernetes.io/managed-by=pyo --ignore-not-found >/dev/null 2>&1 || true
  if [ -n "$remove_cm" ]; then
    echo "Removing cert-manager…"
    act -n kube-system delete helmchart.helm.cattle.io cert-manager --ignore-not-found --wait=false >/dev/null
    gone 300 -n kube-system helmchart.helm.cattle.io cert-manager --ignore-not-found || echo "cert-manager's chart is still being removed. Carrying on." >&2
    has namespace cert-manager && del_ns cert-manager
    act delete crd -l app.kubernetes.io/instance=cert-manager,app.kubernetes.io/name=cert-manager --ignore-not-found >/dev/null
  fi

  # 6. kube-system: join tokens, the GPU device plugin, the CoreDNS budget,
  # the agent's role there. Then its cluster roles, and pyo-system.
  echo "Removing the agent's roles…"
  act -n kube-system delete secret,daemonset,poddisruptionbudget,rolebinding,role -l app.kubernetes.io/managed-by=pyo --ignore-not-found >/dev/null
  act delete clusterrolebinding,clusterrole -l app.kubernetes.io/part-of=pyo-agent --ignore-not-found >/dev/null
  del_ns pyo-system

  # 7. Volumes left Released by what was deleted (a Retain policy keeps
  # their data on the server's disk; the object goes), but those kept.
  local phase kept_pv=()
  while read -r pv phase claim policy path; do
    [ "$phase" = Released ] || continue
    case "$removed_ns" in *" $claim "*) ;; *) continue ;; esac
    [ "${#retain[@]}" -eq 0 ] || case " ${retain[*]%%|*} " in *" $pv "*) continue ;; esac
    act delete pv "$pv" --ignore-not-found >/dev/null
    [ "$policy" != Retain ] || kept_pv+=("$pv${path:+ ($path)}")
  done < <(kc get pv -o jsonpath='{range .items[*]}{.metadata.name} {.status.phase} {.spec.claimRef.namespace} {.spec.persistentVolumeReclaimPolicy} {.spec.local.path}{.spec.hostPath.path}{"\n"}{end}' 2>/dev/null || true)

  echo
  if [ -n "$DRY_RUN" ]; then
    echo "Dry run done: nothing was changed."
  else
    echo "Removed Pyo from this cluster. The cluster and everything else in it stay."
  fi
  if [ "${#kept_pv[@]}" -gt 0 ]; then
    echo "These volumes kept their data on the server's disk (Retain). Delete it there when you're done:"
    printf '  %s\n' "${kept_pv[@]}"
  fi
  if [ -n "$KEEP_DATA" ] && [ "${#projects[@]}" -gt 0 ]; then
    echo "Kept: ${projects[*]}. To delete one later: kubectl delete namespace <name>"
  fi
  if [ "${#retain[@]}" -gt 0 ] && [ -z "$DRY_RUN" ]; then
    local names=() k=kubectl
    [ "${KC[0]}" != k3s ] || k="sudo k3s kubectl"
    echo "These volumes' data was kept on the servers' disks:"
    for line in "${retain[@]}"; do
      IFS='|' read -r pv claim policy path node <<<"$line"
      names+=("$pv")
      echo "  $claim: PersistentVolume $pv${path:+, $path}${node:+ on $node}"
    done
    echo "To delete it later: $k delete pv ${names[*]}"
    echo "then, on each server, remove its directory: sudo rm -rf <path above>"
  fi
}
if [ -n "$UNINSTALL" ]; then
  uninstall
  exit 0
fi
# Without them, on a cluster that's already connected: bring its agent's
# install up to date with Pyo (Pyo shows this command when it's older).
if [ -z "$API" ] && [ -z "$TOKEN" ] && command -v k3s >/dev/null; then
  secret() { k3s kubectl -n pyo-system get secret pyo-agent -o "jsonpath={.data.$1}" 2>/dev/null | base64 -d 2>/dev/null || true; }
  API=$(secret PYO_API_URL); API="${API%/}"; TOKEN=$(secret PYO_AGENT_TOKEN)
  # The cluster Pyo runs on: its agent reaches Pyo inside the cluster, this
  # machine at Pyo's local address.
  case "$API" in http://*.svc:* | http://*.svc.cluster.local:*) API=http://127.0.0.1:8000 ;; esac
  [ -z "$API" ] || echo "Updating this cluster's Pyo agent (Pyo at $API)."
fi
[ -n "$API" ] && [ -n "$TOKEN" ] || { echo "Usage: connect.sh --api URL --token TOKEN [--tailscale-key KEY] [--registry HOST:PORT --registry-token TOKEN]" >&2; exit 2; }
[ -z "$REGISTRY" ] || [ -n "$REGISTRY_TOKEN" ] || { echo "--registry needs --registry-token: copy the full command from Pyo." >&2; exit 2; }
case "$REGISTRY_TOKEN" in *[!0-9a-fA-F]*) echo "That registry token doesn't look like one from Pyo." >&2; exit 2 ;; esac
# The agent's token and its commands' secrets travel to --api: plain http
# only to a private address (this machine, a private network, the tailnet,
# the cluster), never across the internet.
api_ok() {
  case "$1" in
    https://?*) return 0 ;;
    http://*) ;;
    *) return 1 ;;
  esac
  local h="${1#http://}"
  h="${h%%[/?#]*}"; h="${h##*@}"
  case "$h" in
    \[*) h="${h#[}"; h="${h%%]*}" ;;
    *) h="${h%%:*}" ;;
  esac
  h=$(printf '%s' "$h" | tr 'A-Z' 'a-z'); h="${h%.}"
  if [[ "$h" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]]; then
    case "$h" in
      127.* | 10.* | 192.168.*) return 0 ;;
      172.1[6-9].* | 172.2[0-9].* | 172.3[01].*) return 0 ;;
      100.6[4-9].* | 100.[7-9][0-9].* | 100.1[01][0-9].* | 100.12[0-7].*) return 0 ;;
    esac
    return 1
  fi
  case "$h" in
    localhost | *.localhost | *.svc | *.svc.* | *.cluster.local | *.ts.net | ::1 | fc*:* | fd*:*) return 0 ;;
  esac
  return 1
}
api_ok "$API" || { echo "Pyo's address must be https:// ($API isn't on a private network or the tailnet)." >&2; exit 2; }
[ "$(id -u)" = 0 ] || { echo "Run it as root (sudo)." >&2; exit 1; }
[ "$(uname -s)" = Linux ] || { echo "Pyo clusters run on Linux, and this is $(uname -s)." >&2; exit 1; }
case "$(uname -m)" in
  x86_64 | amd64) ;;
  *) echo "Pyo's agent is built for x86-64 (amd64), and this machine is $(uname -m)." >&2; exit 1 ;;
esac
command -v curl >/dev/null || { echo "curl is missing. Install it and run this again." >&2; exit 1; }
case "$TOKEN" in *[!0-9a-fA-F]* | "") echo "That token doesn't look like one from Pyo (letters a-f and digits only)." >&2; exit 2 ;; esac

# >>> pyo:k3s-pin (deploy/k3s.env)
PYO_K3S_VERSION=v1.37.2+k3s1
PYO_K3S_INSTALLER_SHA256=ed01f89fd977bf20ac1516bbebf8370bf3ddbaa55dac8aba610956a4c78cc00b
PYO_K3S_SHA256_AMD64=265b3ee6a3198ef14033c63ef46cbb32370d83795199fb6d1b01f21575eddbda
PYO_K3S_SHA256_ARM64=a779ccf1e5e2e33a62f5a29c2f1c0bbf99431e100579d4c10f0aa0742a6aa84b
# <<< pyo:k3s-pin

# >>> pyo:k3s (the same in deploy/install.sh, connect.sh, join.sh, scripts/setup-host.sh and scripts/join-node.sh)
# k3s, checked before anything is installed: k3s's own installer at the
# tag of the pinned version (PYO_K3S_VERSION) against its SHA-256, and the
# binary against the pinned SHA-256 for this architecture, or, for another
# version (a cluster that runs one), against that GitHub release's checksum
# list. k3s_fetch DIR VERSION downloads and checks; k3s_install DIR ARGS...
# puts the binary in /usr/local/bin and runs the installer with ARGS
# (INSTALL_K3S_EXEC, K3S_URL and K3S_TOKEN pass through).
k3s_fail() {
  echo "Error: $*" >&2
  exit 1
}
k3s_get() { curl -fsSL --proto '=https' --retry 3 --max-time 600 -o "$2" "$1" || k3s_fail "Couldn't download $1."; }
k3s_sha_ok() { [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }
k3s_fetch() {
  local dir=$1 version=$2 arch bin want rel
  case "$(uname -m)" in
    x86_64 | amd64) arch=amd64 bin=k3s want=${PYO_K3S_SHA256_AMD64:-} ;;
    aarch64 | arm64) arch=arm64 bin=k3s-arm64 want=${PYO_K3S_SHA256_ARM64:-} ;;
    *) k3s_fail "There's no k3s for $(uname -m)." ;;
  esac
  [[ ${PYO_K3S_VERSION:-} =~ ^v[0-9]+\.[0-9]+\.[0-9]+\+k3s[0-9]+$ ]] && [[ ${PYO_K3S_INSTALLER_SHA256:-} =~ ^[0-9a-f]{64}$ ]] ||
    k3s_fail "This script doesn't say which k3s to install."
  [[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+\+k3s[0-9]+$ ]] || k3s_fail "\"$version\" isn't a k3s version, like $PYO_K3S_VERSION."
  rel="https://github.com/k3s-io/k3s/releases/download/${version//+/%2B}"
  mkdir -p "$dir"
  echo "Downloading k3s $version and checking it…"
  k3s_get "https://raw.githubusercontent.com/k3s-io/k3s/${PYO_K3S_VERSION//+/%2B}/install.sh" "$dir/install.sh"
  k3s_sha_ok "$dir/install.sh" "$PYO_K3S_INSTALLER_SHA256" ||
    k3s_fail "The downloaded k3s installer doesn't match the expected checksum, so it won't run."
  if [ "$version" != "$PYO_K3S_VERSION" ]; then
    k3s_get "$rel/sha256sum-$arch.txt" "$dir/sha256sum.txt"
    want=$(awk -v f="$bin" '$2 == f { print $1 }' "$dir/sha256sum.txt")
  fi
  [[ $want =~ ^[0-9a-f]{64}$ ]] || k3s_fail "There's no checksum for k3s $version ($bin)."
  k3s_get "$rel/$bin" "$dir/k3s"
  k3s_sha_ok "$dir/k3s" "$want" ||
    k3s_fail "The downloaded k3s binary doesn't match the expected checksum, so it won't be installed."
  chmod 755 "$dir/k3s"
}
k3s_install() {
  local dir=$1
  shift
  [ -x "$dir/k3s" ] && [ -f "$dir/install.sh" ] || k3s_fail "k3s wasn't downloaded."
  install -m 755 "$dir/k3s" /usr/local/bin/.k3s.new
  mv -f /usr/local/bin/.k3s.new /usr/local/bin/k3s
  INSTALL_K3S_SKIP_DOWNLOAD=binary INSTALL_K3S_BIN_DIR=/usr/local/bin sh "$dir/install.sh" "$@"
}
# <<< pyo:k3s
# >>> pyo:small-machine (the same in deploy/install.sh, connect.sh, join.sh, scripts/setup-host.sh and scripts/join-node.sh)
# Run before k3s is installed, or on a k3s that's already installed with
# k3s_memory_refresh. What follows this machine's memory, worked out again
# at every boot by /usr/local/lib/pyo-memory.sh, so a machine given more
# or less memory gets the matching settings when it restarts.
# Under 3.5 GB of memory: k3s gets a soft memory limit, GOMEMLIMIT
# (gomemlimit in the helper), so its Go runtime collects garbage sooner
# instead of growing. Under 2.75 GB, also compressed swap in memory
# (zram, half the memory, pyo-zram.service) for what runs outside
# containers, k3s itself above all (containers don't swap), unless the
# machine has swap of its own. From 3.5 GB, neither. At any size, kubelet
# sets aside memory for k3s and the system (kube-reserved and
# system-reserved, in a k3s config drop-in), so the scheduler only hands
# containers what's left: 256 MiB under 1.5 GB, 512 MiB under 3.5 GB,
# 768 MiB under 8 GB, 1 GiB from there.
# K3S_UNIT: k3s's systemd unit, k3s (a server) unless the script sets
# k3s-agent first (a server joined to a cluster).
K3S_UNIT=${K3S_UNIT:-k3s}
K3S_MEMORY_CONFIG=/etc/rancher/k3s/config.yaml.d/50-pyo-memory.yaml
K3S_MEMORY_CHANGED=""
small_machine() {
  local env before zram_was=""
  # What k3s runs with now: kubelet's reservations and its GOMEMLIMIT.
  before=$(cat "$K3S_MEMORY_CONFIG" /run/pyo-k3s.env 2>/dev/null || true)
  mkdir -p "/etc/systemd/system/$K3S_UNIT.service.d"
  cat > /usr/local/lib/pyo-memory.sh <<'MEMORY'
#!/bin/sh
# Managed by Pyo: k3s's GOMEMLIMIT, kubelet's reservations and compressed
# swap (zram), from this machine's memory at every boot.
mem=$(awk '/^MemTotal:/ { print int($2 / 1024) }' "${PYO_MEMINFO:-/proc/meminfo}")
# k3s's soft memory limit in MiB: 40% of the memory, at least 768 MiB
# (k3s's own heap is 330 to 400 MB: a lower limit keeps its garbage
# collector running all the time).
gomemlimit() {
  limit=$((mem * 2 / 5))
  [ "$limit" -ge 768 ] || limit=768
  echo "$limit"
}
# What kubelet sets aside in MiB, for k3s (kube-reserved: its one process
# holds 500 to 600 MB on a small machine) and for the system
# (system-reserved: sshd, tailscaled, journald, …).
reserved() {
  if [ "$mem" -lt 1536 ]; then
    echo "192 64"
  elif [ "$mem" -lt 3584 ]; then
    echo "384 128"
  elif [ "$mem" -lt 8192 ]; then
    echo "512 256"
  else
    echo "768 256"
  fi
}
case "$1" in
  k3s-env)
    # For k3s's EnvironmentFile: GOMEMLIMIT under 3.5 GB, else nothing.
    if [ "${mem:-0}" -gt 0 ] && [ "$mem" -lt 3584 ]; then
      echo "GOMEMLIMIT=$(gomemlimit)MiB"
    fi
    ;;
  k3s-config)
    # k3s's config drop-in ($2): kubelet's reservations; none (no file)
    # when the memory can't be read.
    [ -n "$2" ] || exit 2
    if [ "${mem:-0}" -gt 0 ]; then
      set -- "$2" $(reserved)
      mkdir -p "$(dirname "$1")"
      printf '%s\n' "# Managed by Pyo: what kubelet sets aside, from this machine's memory at each start." \
        'kubelet-arg+:' "  - \"kube-reserved=memory=$2Mi\"" "  - \"system-reserved=memory=$3Mi\"" > "$1.new" && mv -f "$1.new" "$1"
    else
      rm -f "$2"
    fi
    ;;
  zram-start)
    [ "${mem:-0}" -gt 0 ] && [ "$mem" -lt 2816 ] || exit 0
    [ -z "$(swapon --show=NAME --noheadings 2>/dev/null)" ] || exit 0
    command -v zramctl >/dev/null && modprobe zram 2>/dev/null || exit 0
    d=$(zramctl --find --size "$((mem / 2))MiB") && mkswap "$d" >/dev/null && swapon -p 100 "$d" && echo "$d" > /run/pyo-zram
    ;;
  zram-stop)
    [ -f /run/pyo-zram ] || exit 0
    d=$(cat /run/pyo-zram)
    swapoff "$d"
    for i in 1 2 3 4 5; do
      zramctl --reset "$d" 2>/dev/null && break
      sleep 1
    done
    rm -f /run/pyo-zram
    ;;
esac
MEMORY
  chmod 755 /usr/local/lib/pyo-memory.sh
  printf '%s\n' '# Managed by Pyo: GOMEMLIMIT on a small machine and what kubelet sets aside, worked out at each start.' '[Service]' \
    "ExecStartPre=-/bin/sh -c '/usr/local/lib/pyo-memory.sh k3s-env > /run/pyo-k3s.env'" \
    "ExecStartPre=-/usr/local/lib/pyo-memory.sh k3s-config $K3S_MEMORY_CONFIG" \
    'EnvironmentFile=-/run/pyo-k3s.env' > "/etc/systemd/system/$K3S_UNIT.service.d/pyo-memory.conf"
  printf '%s\n' '[Unit]' 'Description=Pyo: compressed swap in memory (zram) on a small machine' "Before=$K3S_UNIT.service" \
    '[Service]' 'Type=oneshot' 'RemainAfterExit=yes' \
    'ExecStart=/usr/local/lib/pyo-memory.sh zram-start' 'ExecStop=/usr/local/lib/pyo-memory.sh zram-stop' \
    '[Install]' 'WantedBy=multi-user.target' > /etc/systemd/system/pyo-zram.service
  systemctl daemon-reload
  # Said only when it turns on here, not when it's on already.
  [ ! -f /run/pyo-zram ] || zram_was=1
  systemctl enable --now pyo-zram.service >/dev/null 2>&1 || echo "Couldn't set up compressed swap (zram); going on without it." >&2
  env=$(/usr/local/lib/pyo-memory.sh k3s-env)
  [ -z "$env" ] || echo "A small machine: k3s gets $env."
  /usr/local/lib/pyo-memory.sh k3s-config "$K3S_MEMORY_CONFIG" || true
  [ -n "$zram_was" ] || [ ! -f /run/pyo-zram ] || echo "Turned on compressed swap in memory (zram)."
  [ "$before" = "$(cat "$K3S_MEMORY_CONFIG" 2>/dev/null; [ -z "$env" ] || echo "$env")" ] || K3S_MEMORY_CHANGED=1
}
# The same on a k3s that's already installed ($K3S_UNIT.service). k3s uses what
# changed once it restarts: it restarts here, unless $1 is "later" (the
# caller restarts it, when K3S_MEMORY_CHANGED is set). Containers keep
# running while k3s restarts.
k3s_memory_refresh() {
  systemctl cat "$K3S_UNIT.service" >/dev/null 2>&1 || return 0
  K3S_MEMORY_CHANGED=""
  small_machine
  [ -n "$K3S_MEMORY_CHANGED" ] || return 0
  echo "k3s restarts briefly to apply its memory reserve. Services keep running."
  [ "${1:-}" = later ] || systemctl restart "$K3S_UNIT"
}
# <<< pyo:small-machine

# >>> pyo:repos (the same in connect.sh, join.sh and scripts/join-node.sh)
# Packages come from their vendors' repositories, whose signing keys are
# checked by fingerprint here: Tailscale's and NVIDIA's.
TAILSCALE_KEY=2596A99EAAB33821893C0A79458CA832957F5868
# shellcheck disable=SC2034 # for the GPU setup, which not every script has
NVIDIA_KEY=C95B321B61E88C1809C4F759DDCAE044F796ECB0
os_field() { sed -n "s/^$1=//p" /etc/os-release 2>/dev/null | tr -d '"' | head -n1; }
# signing_key URL FINGERPRINT FILE [armor]: the key with that fingerprint
# from URL into FILE (only that key), or an error.
signing_key() {
  local url=$1 fpr=$2 out=$3 g ok="" pm
  if ! command -v gpg >/dev/null; then
    if command -v apt-get >/dev/null; then
      apt-get update -qq && apt-get install -y -qq gnupg >/dev/null
    elif pm=$(command -v dnf || command -v yum); then
      "$pm" install -y -q gnupg2 >/dev/null
    fi || true
    command -v gpg >/dev/null || { echo "gpg is missing, so the signing key at $url can't be checked. Install gnupg and run this again." >&2; return 1; }
  fi
  g=$(mktemp -d)
  if ! curl -fsSL --proto '=https' --retry 3 --max-time 60 -o "$g/key" "$url"; then
    echo "Couldn't download the signing key $url." >&2
  elif GNUPGHOME=$g gpg --batch --quiet --import "$g/key" 2>/dev/null &&
    GNUPGHOME=$g gpg --batch --with-colons --fingerprint "$fpr" 2>/dev/null | grep -qx "fpr:::::::::$fpr:" &&
    GNUPGHOME=$g gpg --batch ${4:+--armor} --export "$fpr" > "$g/out" && [ -s "$g/out" ]; then
    mkdir -p "$(dirname "$out")" && install -m 644 "$g/out" "$out" && ok=1
  else
    echo "The signing key at $url isn't the expected one ($fpr), so it won't be used." >&2
  fi
  GNUPGHOME=$g gpgconf --kill all >/dev/null 2>&1 || true
  rm -rf "$g"
  [ -n "$ok" ]
}
# Tailscale, unless it's installed: from Tailscale's repository with apt
# (Debian, Ubuntu and their derivatives) or dnf/yum (Fedora, RHEL and
# RHEL-compatible systems).
tailscale_install() {
  command -v tailscale >/dev/null && return 0
  local repo=https://pkgs.tailscale.com/stable id path="" code="" pm
  local other="Install Tailscale (https://tailscale.com/download/linux) and run this again."
  id=$(os_field ID)
  echo "Installing Tailscale from its package repository…"
  if command -v apt-get >/dev/null; then
    case "$id" in
      ubuntu | debian) path=$id code=$(os_field VERSION_CODENAME) ;;
      *)
        code=$(os_field UBUNTU_CODENAME) path=ubuntu
        [ -n "$code" ] || { code=$(os_field DEBIAN_CODENAME) path=debian; }
        ;;
    esac
    [ -n "$code" ] || { echo "Pyo can't tell which Debian or Ubuntu release this is. $other" >&2; exit 1; }
    signing_key "$repo/$path/$code.noarmor.gpg" "$TAILSCALE_KEY" /usr/share/keyrings/tailscale-archive-keyring.gpg || exit 1
    echo "deb [signed-by=/usr/share/keyrings/tailscale-archive-keyring.gpg] $repo/$path $code main" > /etc/apt/sources.list.d/tailscale.list
    apt-get update -qq && apt-get install -y -qq tailscale >/dev/null
  elif pm=$(command -v dnf || command -v yum); then
    case "$id" in
      fedora) path=fedora ;;
      rhel | centos | rocky | almalinux | ol) path=rhel/$(os_field VERSION_ID | cut -d. -f1) ;;
      *) echo "Pyo installs Tailscale on Fedora and RHEL-compatible systems, not $id. $other" >&2; exit 1 ;;
    esac
    signing_key "$repo/$path/repo.gpg" "$TAILSCALE_KEY" /etc/pki/rpm-gpg/RPM-GPG-KEY-tailscale armor || exit 1
    cat > /etc/yum.repos.d/tailscale.repo <<REPO
[tailscale-stable]
name=Tailscale stable
baseurl=$repo/$path/\$basearch
enabled=1
type=rpm
repo_gpgcheck=1
gpgcheck=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-tailscale
REPO
    "$pm" install -y -q tailscale
  else
    echo "Pyo installs Tailscale with apt, dnf or yum, and this system has none. $other" >&2
    exit 1
  fi
  command -v tailscale >/dev/null || { echo "Tailscale didn't install (see above)." >&2; exit 1; }
}
# <<< pyo:repos

# Tailscale, when Pyo is on your tailnet (or this machine already is).
# TS_NAME: this machine's MagicDNS name, when the tailnet has MagicDNS.
TS_IP="" TS_NAME=""
if [ -n "$TS_AUTHKEY" ] || command -v tailscale >/dev/null; then
  tailscale_install
  systemctl enable --now tailscaled
  if [ -z "$(tailscale ip -4 2>/dev/null)" ]; then
    [ -n "$TS_AUTHKEY" ] || { echo "Not on the tailnet yet: add --tailscale-key tskey-auth-..." >&2; exit 1; }
    tailscale up --authkey "$TS_AUTHKEY" --hostname "$(hostname)"
  fi
  TS_IP=$(tailscale ip -4 | head -n1)
  TS_NAME=$(tailscale status --json 2>/dev/null | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1 || true)
  TS_NAME=${TS_NAME%.}
fi

# >>> pyo:tailnet-host (the same in connect.sh and join.sh)
# A tailnet machine's address, from the MagicDNS name Pyo's commands give
# ("<host>.<tailnet>.ts.net"): its IPv4 from Tailscale's own list of the
# tailnet's machines, so it needn't resolve through DNS here, else from DNS.
# What k3s, the agent and the registry login keep is then the IP, which
# needs no DNS later either. An IP, or a name neither knows, as it is.
tailnet_host() {
  local h=$1 ip=""
  case "$h" in
    *.ts.net | *.ts.net.) ;;
    *) printf '%s\n' "$h"; return 0 ;;
  esac
  if command -v tailscale >/dev/null; then
    ip=$(tailscale ip -4 "${h%.}." 2>/dev/null | head -n1 || true)
  fi
  [ -n "$ip" ] || ip=$(getent ahostsv4 "${h%.}" 2>/dev/null | awk 'NR == 1 { print $1 }' || true)
  printf '%s\n' "${ip:-$h}"
}
# "host:port" with the host as tailnet_host has it.
tailnet_hostport() {
  case "$1" in
    *:*) printf '%s:%s\n' "$(tailnet_host "${1%:*}")" "${1##*:}" ;;
    *) tailnet_host "$1" ;;
  esac
}
# <<< pyo:tailnet-host

# Pyo by its tailnet name, now that this machine is on the tailnet: dialled
# at its IP, which is also the address the agent is given (pods needn't
# resolve MagicDNS names). Its registry door the same way.
case "$API" in
  http://*)
    api_hostport=${API#http://}; api_hostport=${api_hostport%%/*}
    API="http://$(tailnet_hostport "$api_hostport")${API#"http://$api_hostport"}"
    ;;
esac
[ -z "$REGISTRY" ] || REGISTRY=$(tailnet_hostport "$REGISTRY")

# Pyo answers, and knows this token, before anything is installed.
MANIFEST=$(mktemp) CRDS=$(mktemp) K3S_DIR=$(mktemp -d)
trap 'rm -rf "$MANIFEST" "$CRDS" "$K3S_DIR"' EXIT
code=$(curl -sS -o "$MANIFEST" -w '%{http_code}' --max-time 30 "$API/install/agent.yaml?token=$TOKEN" || true)
case "$code" in
  200) ;;
  404) echo "Pyo at $API doesn't know this token. Was the cluster removed, or the token rotated? Copy a fresh command from Pyo." >&2; exit 1 ;;
  000) echo "Can't reach Pyo at $API from this machine. Is it on the same network (or tailnet), and is port ${API##*:} open?" >&2; exit 1 ;;
  *) echo "Pyo at $API answered $code for the agent manifest." >&2; exit 1 ;;
esac

# Firewall: web traffic from anywhere, cluster traffic over the tailnet.
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow 80/tcp comment 'Pyo ingress' >/dev/null
  ufw allow 443/tcp comment 'Pyo ingress' >/dev/null
  [ -z "$TS_IP" ] || ufw allow in on tailscale0 comment 'k3s over Tailscale' >/dev/null
fi

# AppArmor: Ubuntu with kernel 6.17+ and
# kernel.apparmor_restrict_unprivileged_unconfined=1 stacks container
# profiles, and containerd's default one then stops processes in a container
# signalling each other (Postgres, Go programs and CoreDNS hang). Install a
# corrected copy before k3s starts, unless someone else's profile is there.
# https://github.com/containerd/containerd/issues/14485
# The profile below is crates/pyo-api/assets/cri-containerd.apparmor.d.
apparmor_workaround() {
  command -v apparmor_parser >/dev/null || return 0
  [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_unconfined 2>/dev/null)" = 1 ] || return 0
  local kv maj min f=/etc/apparmor.d/cri-containerd.apparmor.d
  kv=$(uname -r); maj=${kv%%.*}; min=${kv#*.}; min=${min%%.*}
  [ "$maj" -gt 6 ] || { [ "$maj" -eq 6 ] && [ "$min" -ge 17 ]; } || return 0
  if [ -f "$f" ] && ! head -n1 "$f" | grep -q '^# Managed by Pyo'; then
    echo "Note: $f isn't Pyo's, so it's left alone. On this kernel, containers may hang:" >&2
    echo "  https://github.com/containerd/containerd/issues/14485" >&2
    return 0
  fi
  cat > "$f" <<'PYO_APPARMOR_PROFILE'
# Managed by Pyo. Remove this file and restart k3s once k3s ships a
# containerd with the fix below.
#
# containerd's default container profile (contrib/apparmor/template.go in
# containerd v2.3.4, as bundled with k3s v1.36.5), with two lines added.
# On Ubuntu with kernel.apparmor_restrict_unprivileged_unconfined=1 and
# kernel 6.17+/7.0, container processes run under the stacked label
# "cri-containerd.apparmor.d//&unconfined", which the stock
# "peer=cri-containerd.apparmor.d" rules don't match: processes in the same
# container can't signal each other (Postgres, Go programs, CoreDNS hang).
# https://github.com/containerd/containerd/issues/14485
# https://github.com/k3s-io/k3s/issues/13625
#
# AppArmor loads this at boot, before k3s; containerd then keeps the
# loaded profile instead of generating its own.

abi <abi/3.0>,

#include <tunables/global>

profile cri-containerd.apparmor.d flags=(attach_disconnected,mediate_deleted) {
  #include <abstractions/base>

  network,
  capability,
  file,
  umount,
  # Host (privileged) processes may send signals to container processes.
  signal (receive) peer=unconfined,
  # runc may send signals to container processes.
  signal (receive) peer=runc,
  # crun may send signals to container processes.
  signal (receive) peer=crun,
  # Manager may send signals to container processes.
  signal (receive) peer=unconfined,
  # Container processes may send signals amongst themselves.
  signal (send,receive) peer=cri-containerd.apparmor.d,
  # Pyo: the same, for the stacked label (the bug above).
  signal (send,receive) peer=cri-containerd.apparmor.d//&unconfined,

  deny @{PROC}/* w,   # deny write for all files directly in /proc (not in a subdir)
  # deny write to files not in /proc/<number>/** or /proc/sys/**
  deny @{PROC}/{[^1-9/],[^1-9/][^0-9/],[^1-9s/][^0-9y/][^0-9s/],[^1-9/][^0-9/][^0-9/][^0-9/]*}/** w,
  deny @{PROC}/sys/[^k]** w,  # deny /proc/sys except /proc/sys/k* (effectively /proc/sys/kernel)
  deny @{PROC}/sys/kernel/{?,??,[^s][^h][^m]**} w,  # deny everything except shm* in /proc/sys/kernel/
  deny @{PROC}/sysrq-trigger rwklx,
  deny @{PROC}/kcore rwklx,

  deny mount,

  deny /sys/[^f]*/** wklx,
  deny /sys/f[^s]*/** wklx,
  deny /sys/fs/[^c]*/** wklx,
  deny /sys/fs/c[^g]*/** wklx,
  deny /sys/fs/cg[^r]*/** wklx,
  deny /sys/firmware/** rwklx,
  deny /sys/devices/virtual/powercap/** rwklx,
  deny /sys/kernel/security/** rwklx,

  # allow processes within the container to trace each other,
  # provided all other LSM and yama setting allow it.
  ptrace (trace,tracedby,read,readby) peer=cri-containerd.apparmor.d,
  # Pyo: the same, for the stacked label.
  ptrace (trace,tracedby,read,readby) peer=cri-containerd.apparmor.d//&unconfined,
}
PYO_APPARMOR_PROFILE
  chmod 0644 "$f"
  apparmor_parser -r "$f"
  echo "Applied Pyo's AppArmor workaround for kernel $kv ($f)."
}
apparmor_workaround

# NVIDIA GPU: containers use it through the NVIDIA Container Toolkit, which
# k3s looks for when it starts (and then has an "nvidia" runtime). The
# driver is never installed here: that's yours to install (and reboot for).
# The toolkit is installed from NVIDIA's repository on apt and dnf/yum
# systems; elsewhere this says what's missing. Sets GPU_LABELS when the GPU
# is ready for containers, and GPU_TOOLKIT_NEW when the toolkit was just
# installed (a k3s that's already running has to restart to see it). A GPU
# that can't be used yet is labelled pyo/gpu=no-driver or no-toolkit, so
# Pyo can say what's missing.
GPU_LABELS="" GPU_TOOLKIT_NEW=""
nvidia_gpu() {
  local d found="" model mem pm
  for d in /sys/bus/pci/devices/*; do
    [ "$(cat "$d/vendor" 2>/dev/null)" = 0x10de ] || continue
    case "$(cat "$d/class" 2>/dev/null)" in 0x0300*|0x0302*) found=1 ;; esac
  done
  [ -n "$found" ] || return 0
  if ! nvidia-smi -L >/dev/null 2>&1; then
    echo "This machine has an NVIDIA GPU, but no working driver (nvidia-smi fails), so Pyo won't use it." >&2
    echo "  Install the driver, reboot, and run this again. On Ubuntu: sudo ubuntu-drivers install" >&2
    echo "  Elsewhere: https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/" >&2
    echo "  Carrying on without the GPU." >&2
    GPU_LABELS="pyo/gpu=no-driver"
    return 0
  fi
  if ! command -v nvidia-container-runtime >/dev/null; then
    echo "Installing the NVIDIA Container Toolkit, so containers can use the GPU..."
    if command -v apt-get >/dev/null; then
      if signing_key https://nvidia.github.io/libnvidia-container/gpgkey "$NVIDIA_KEY" /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg; then
        # shellcheck disable=SC2016 # $(ARCH) is apt's
        echo 'deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://nvidia.github.io/libnvidia-container/stable/deb/$(ARCH) /' \
          > /etc/apt/sources.list.d/nvidia-container-toolkit.list
        apt-get update -qq && apt-get install -y -qq nvidia-container-toolkit >/dev/null
      fi
    elif pm=$(command -v dnf || command -v yum); then
      if signing_key https://nvidia.github.io/libnvidia-container/gpgkey "$NVIDIA_KEY" /etc/pki/rpm-gpg/RPM-GPG-KEY-nvidia-container-toolkit armor; then
        cat > /etc/yum.repos.d/nvidia-container-toolkit.repo <<'REPO'
[nvidia-container-toolkit]
name=nvidia-container-toolkit
baseurl=https://nvidia.github.io/libnvidia-container/stable/rpm/$basearch
repo_gpgcheck=1
gpgcheck=0
enabled=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-nvidia-container-toolkit
REPO
        "$pm" install -y -q nvidia-container-toolkit
      fi
    else
      echo "Pyo can't install the NVIDIA Container Toolkit on this system. Install it" >&2
      echo "  (https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)" >&2
      echo "  and run this again. Carrying on without the GPU." >&2
      GPU_LABELS="pyo/gpu=no-toolkit"
      return 0
    fi
    command -v nvidia-container-runtime >/dev/null || { echo "The NVIDIA Container Toolkit didn't install. Carrying on without the GPU." >&2; GPU_LABELS="pyo/gpu=no-toolkit"; return 0; }
    GPU_TOOLKIT_NEW=1
  fi
  # The model as a label value: "NVIDIA GeForce RTX 4090" -> NVIDIA-GeForce-RTX-4090.
  model=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -n1 | tr -c 'A-Za-z0-9._-' '-' | cut -c1-63 | sed 's/--*/-/g; s/^[-._]*//; s/[-._]*$//')
  mem=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null | head -n1 | tr -dc 0-9)
  GPU_LABELS="pyo/gpu=nvidia${model:+ pyo/gpu-model=$model}${mem:+ pyo/gpu-memory-mib=$mem}"
  echo "NVIDIA GPU ready for containers: $(nvidia-smi -L | head -n1)"
}
nvidia_gpu

# k3s server, unless this machine already runs k3s.
mkdir -p /etc/rancher/k3s
if [ -n "$REGISTRY" ]; then
  # Images Pyo builds, through its registry door with this cluster's login.
  (umask 077 && cat > /etc/rancher/k3s/registries.yaml <<REG
mirrors:
  "127.0.0.1:5005":
    endpoint:
      - "http://$REGISTRY"
configs:
  "$REGISTRY":
    auth:
      username: pyo-cluster
      password: "$REGISTRY_TOKEN"
REG
  )
  chmod 600 /etc/rancher/k3s/registries.yaml
fi
if ! command -v k3s >/dev/null; then
  # Pyo's k3s (deploy/k3s.env), downloaded and checked before it's set up.
  k3s_fetch "$K3S_DIR" "$PYO_K3S_VERSION"
  # Secrets are encrypted in the datastore with secretbox (XSalsa20-Poly1305).
  # Pyo's gateway (Envoy Gateway, from the agent's manifest) answers
  # on 80/443, not k3s's Traefik.
  cat > /etc/rancher/k3s/config.yaml <<CFG
secrets-encryption: true
secrets-encryption-provider: secretbox
disable:
  - traefik
CFG
  if [ -n "$GPU_LABELS" ]; then
    echo "node-label:" >> /etc/rancher/k3s/config.yaml
    for l in $GPU_LABELS; do echo "  - \"$l\"" >> /etc/rancher/k3s/config.yaml; done
  fi
  if [ -n "$TS_IP" ]; then
    cat >> /etc/rancher/k3s/config.yaml <<CFG
node-ip: "$TS_IP"
flannel-iface: tailscale0
tls-san:
  - "$TS_IP"
CFG
    # The Kubernetes API's certificate also names the machine's MagicDNS name.
    [ -z "$TS_NAME" ] || echo "  - \"$TS_NAME\"" >> /etc/rancher/k3s/config.yaml
  fi
  small_machine
  k3s_install "$K3S_DIR" server
else
  # Its memory reserve brought up to date; k3s restarts below if it changed.
  k3s_memory_refresh later
  # k3s finds a newly installed NVIDIA toolkit only when it starts.
  if [ -n "$REGISTRY" ] || [ -n "$GPU_TOOLKIT_NEW" ] || [ -n "$K3S_MEMORY_CHANGED" ]; then systemctl restart k3s 2>/dev/null || true; fi
  # An existing k3s keeps its settings; say how to encrypt its Secrets.
  if ! k3s secrets-encrypt status 2>/dev/null | grep -q "Encryption Status: Enabled"; then
    echo "Note: this k3s doesn't encrypt Secrets at rest. To turn it on:"
    echo "  k3s secrets-encrypt enable && systemctl restart k3s && k3s secrets-encrypt reencrypt"
  fi
fi

echo "Waiting for k3s…"
for _ in $(seq 90); do k3s kubectl get nodes >/dev/null 2>&1 && break; sleep 2; done
k3s kubectl get nodes >/dev/null 2>&1 || { echo "k3s isn't answering. Check: systemctl status k3s" >&2; exit 1; }
# An existing k3s didn't get the labels at registration: add them now.
if [ -n "$GPU_LABELS" ] && k3s kubectl get node "$(hostname)" >/dev/null 2>&1; then
  # shellcheck disable=SC2086 # one label per word
  k3s kubectl label node "$(hostname)" --overwrite $GPU_LABELS >/dev/null || true
fi

# Pyo Gateway's APIs, before the manifest installs Envoy Gateway (it only
# watches kinds whose CRDs exist when it starts): Envoy Gateway's own, and
# the Gateway API's standard channel on a cluster without one. k3s 1.37+
# brings its own Gateway API (and refuses another channel over it): wait for
# that instead. A Gateway API Pyo applied is kept current until k3s brings
# its own; it carries the ownership k3s's chart takes over on upgrade.
gateway_crds() {
  local crd=customresourcedefinition.apiextensions.k8s.io q="" restart=""
  local api="$crd/gateways.gateway.networking.k8s.io"
  if [ -e /var/lib/rancher/k3s/server/manifests/gateway-api-crd.yaml ] || k3s kubectl -n kube-system get helmchart.helm.cattle.io gateway-api-crd >/dev/null 2>&1; then
    if ! k3s kubectl get "$crd/udproutes.gateway.networking.k8s.io" >/dev/null 2>&1; then
      echo "Waiting for k3s's Gateway API…"
      for _ in $(seq 90); do k3s kubectl get "$crd/udproutes.gateway.networking.k8s.io" >/dev/null 2>&1 && break; sleep 2; done
      k3s kubectl wait --for=condition=Established --timeout=60s "$api" "$crd/tcproutes.gateway.networking.k8s.io" "$crd/udproutes.gateway.networking.k8s.io" >/dev/null 2>&1 ||
        echo "k3s hasn't installed the Gateway API yet. Domains start answering once it has." >&2
    fi
  elif k3s kubectl get "$api" >/dev/null 2>&1; then
    [ "$(k3s kubectl get "$api" -o jsonpath='{.metadata.labels.pyo\.dev/gateway-api}' 2>/dev/null || true)" != true ] || q="?gatewayApi=1"
  else
    q="?gatewayApi=1"
  fi
  # TCP and UDP ports need the Gateway API's v1 routes (1.5 or later).
  if [ -z "$q" ] && k3s kubectl get "$api" >/dev/null 2>&1 &&
    [ "$(k3s kubectl get "$crd/tcproutes.gateway.networking.k8s.io" -o jsonpath='{.spec.versions[?(@.name=="v1")].served}' 2>/dev/null || true)" != true ]; then
    echo "Note: this cluster's Gateway API is older than 1.5, so Pyo's proxied TCP and UDP ports won't work." >&2
  fi
  # Envoy Gateway already running without its CRDs: restart it once they're in.
  k3s kubectl get "$crd/backends.gateway.envoyproxy.io" >/dev/null 2>&1 || restart=1
  curl -fsS --max-time 60 -o "$CRDS" "$API/install/gateway-crds.yaml$q" || { echo "Couldn't get the gateway's CRDs from Pyo at $API." >&2; exit 1; }
  k3s kubectl apply --server-side --force-conflicts --field-manager=pyo -f "$CRDS" >/dev/null || { echo "Couldn't install the gateway's CRDs (see above)." >&2; exit 1; }
  if [ -n "$restart" ] && k3s kubectl -n pyo-gateway get deployment envoy-gateway >/dev/null 2>&1; then
    k3s kubectl -n pyo-gateway rollout restart deployment envoy-gateway >/dev/null || true
  fi
}
gateway_crds

# >>> pyo:agent-apply (connect.sh only; join.rs tests it)
# The Kubernetes minor version the API server reports; empty when it doesn't
# answer.
server_minor() {
  { k3s kubectl version -o json 2>/dev/null || true; } |
    sed -n '/"serverVersion"/,/}/s/^ *"minor": *"\([0-9][0-9]*\).*/\1/p'
}
# The Pyo agent and the admission policy that holds it to its limits. Right
# after k3s first starts, or on a small server under load, its API can take
# longer to check the policy than it gives a write by default (34 s): each
# try asks for 2 minutes (the API server caps that at its own limit), as a
# server-side apply (one request per object, without a copy of it in an
# annotation), and tries go on for 5 minutes. Only Kubernetes older than
# 1.30 (it has no admission policies) goes on without the policy.
apply_agent() {
  local minor tries=0 delay=5 start=$SECONDS until=$((SECONDS + 300))
  echo "Installing the Pyo agent and its admission policy…"
  while :; do
    tries=$((tries + 1))
    if k3s kubectl get --raw=/readyz --request-timeout=20s >/dev/null 2>&1 &&
      k3s kubectl apply --server-side --force-conflicts --field-manager=pyo --request-timeout=120s -f "$MANIFEST" >/dev/null; then
      return 0
    fi
    minor=$(server_minor)
    if [ -n "$minor" ] && [ "$minor" -lt 30 ]; then
      echo "Note: this Kubernetes is 1.$minor, older than 1.30, so the agent's admission policy isn't enforced. Its roles still apply."
      return 0
    fi
    [ "$SECONDS" -lt "$until" ] || return 1
    echo "Kubernetes hasn't taken them yet (try $tries, $((SECONDS - start))s so far). Trying again in ${delay}s…" >&2
    sleep "$delay"
    delay=$((delay * 2 > 30 ? 30 : delay * 2))
  done
}
# <<< pyo:agent-apply
# The Pyo agent: it dials out to Pyo; nothing here is exposed.
if ! apply_agent; then
  echo "Couldn't install the Pyo agent and its admission policy (see above), so its limits wouldn't be enforced. Run this again once k3s answers: systemctl status k3s" >&2
  exit 1
fi
echo "Starting the Pyo agent…"
if ! k3s kubectl -n pyo-system rollout status deployment/pyo-agent --timeout=180s >/dev/null; then
  echo "The agent hasn't started yet. Look at: k3s kubectl -n pyo-system get pods" >&2
  exit 1
fi

# How this server shows up: its addresses, Tailscale's included.
addrs=$(ip -o addr show scope global 2>/dev/null |
  awk '$2 !~ /^(lo|docker|br-|veth|cni|flannel|kube|tailscale|vxlan|cali|virbr)/ {split($4, x, "/"); printf "%s ", x[1]}')
echo "Connected $(hostname) (${addrs}${TS_IP:+$TS_IP }${TS_NAME})."
echo "It shows up in Pyo in a few seconds."
