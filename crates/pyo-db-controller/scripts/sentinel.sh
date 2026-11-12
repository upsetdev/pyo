#!/bin/sh
# Starts a sentinel watching the current primary. Addresses only, no names:
# see valkey.sh.
set -eu
svc() { echo "$1.$NAMESPACE.svc.$DOMAIN"; }
addr() { a=""; until [ -n "$a" ]; do a=$(getent hosts "$(svc "$1")" | awk '{print $1; exit}'); [ -n "$a" ] || sleep 1; done; echo "$a"; }
cli() { timeout 3 valkey-cli --no-auth-warning -a "$PASSWORD" "$@"; }
ME=$(addr "$POD_NAME")
master=""
# The primary: what the other sentinels say, else whichever instance says
# it's primary, else -0.
for s in 0 1 2; do
  [ "$NAME-sentinel-$s" = "$POD_NAME" ] && continue
  m=$(cli -h "$(addr "$NAME-sentinel-$s")" -p 26379 sentinel get-master-addr-by-name "$NAME" 2>/dev/null | head -n1 || true)
  if [ -n "$m" ]; then master=$m; break; fi
done
if [ -z "$master" ]; then
  n=0
  while [ $n -lt "$INSTANCES" ]; do
    peer=$(addr "$NAME-$n")
    if [ "$(cli -h "$peer" role 2>/dev/null | head -n1 || true)" = master ]; then master=$peer; break; fi
    n=$((n+1))
  done
fi
[ -n "$master" ] || master=$(addr "$NAME-0")
conf=/conf/sentinel.conf
cat > "$conf" <<CONF
port 26379
sentinel announce-ip $ME
requirepass "$PASSWORD"
sentinel sentinel-pass "$PASSWORD"
sentinel monitor $NAME $master 6379 2
sentinel auth-pass $NAME "$PASSWORD"
sentinel down-after-milliseconds $NAME 5000
sentinel failover-timeout $NAME 30000
sentinel parallel-syncs $NAME 1
CONF
echo "watching $master"
exec valkey-server "$conf" --sentinel
