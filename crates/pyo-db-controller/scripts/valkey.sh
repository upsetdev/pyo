#!/bin/sh
# Starts a Valkey instance as the primary or as a replica of it. With READ=1
# it's a read replica: always a replica, with priority 0 so the sentinels
# never make it the primary (they still move it to a new primary).
#
# Every instance and sentinel has a service named after its pod, with an
# address that stays the same for the Database's whole life (a pod's changes
# on every restart). Instances and sentinels know each other only by those
# addresses: Sentinel blocks while it looks up names, and a slow lookup (a DNS
# server on a server that just died) pauses it right when it should fail over.
set -eu
svc() { echo "$1.$NAMESPACE.svc.$DOMAIN"; }
# The service's address, waiting until it has one.
addr() { a=""; until [ -n "$a" ]; do a=$(getent hosts "$(svc "$1")" | awk '{print $1; exit}'); [ -n "$a" ] || sleep 1; done; echo "$a"; }
# Something that drops packets (a name with no pod yet) is never waited on.
cli() { timeout 3 valkey-cli --no-auth-warning -a "$PASSWORD" "$@"; }
ME=$(addr "$POD_NAME")
master=""
# Who's primary: the sentinels, so an instance that comes back after a
# failover rejoins as a replica...
if [ "$SENTINEL" = 1 ]; then
  try=0
  while [ -z "$master" ] && [ $try -lt 5 ]; do
    for s in 0 1 2; do
      m=$(cli -h "$(addr "$NAME-sentinel-$s")" -p 26379 sentinel get-master-addr-by-name "$NAME" 2>/dev/null | head -n1 || true)
      if [ -n "$m" ]; then master=$m; break; fi
    done
    try=$((try+1))
    [ -n "$master" ] || sleep 2
  done
fi
# ...else whichever other instance says it's primary...
if [ -z "$master" ]; then
  n=0
  while [ $n -lt "$INSTANCES" ]; do
    peer=$(addr "$NAME-$n")
    if [ "$peer" != "$ME" ] && [ "$(cli -h "$peer" role 2>/dev/null | head -n1 || true)" = master ]; then master=$peer; break; fi
    n=$((n+1))
  done
fi
# ...else, on the first start, -0.
[ -n "$master" ] || master=$(addr "$NAME-0")
conf=/data/valkey.conf
cat > "$conf" <<CONF
bind * -::*
port 6379
dir /data
appendonly yes
appendfsync everysec
requirepass "$PASSWORD"
masterauth "$PASSWORD"
replica-announce-ip $ME
maxmemory-policy noeviction
CONF
if [ -n "${MAXMEMORY:-}" ]; then echo "maxmemory $MAXMEMORY" >> "$conf"; fi
if [ "${READ:-0}" = 1 ]; then echo "replica-priority 0" >> "$conf"; fi
# Modules (valkey-bundle's), the same on every instance.
for m in ${MODULES:-}; do echo "loadmodule $m" >> "$conf"; done
if [ "$master" != "$ME" ]; then
  echo "replicaof $master 6379" >> "$conf"
  echo "starting as a replica of $master"
else
  echo "starting as the primary"
fi
exec valkey-server "$conf"
