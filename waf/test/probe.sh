#!/usr/bin/env bash
# probe.sh PORT: a normal request, SQL injection and XSS (query and form
# body) for each test host; prints the status codes.
P=${1:-19183}
for h in block.test log.test off.test except.test pl2.test; do
  c(){ curl -s -o /dev/null -w %{http_code} -H "Host: $h" "$@"; }
  echo "$h normal=$(c http://127.0.0.1:$P/) sqli=$(c "http://127.0.0.1:$P/?id=1%27%20OR%20%271%27=%271") xss=$(c "http://127.0.0.1:$P/?q=%3Cscript%3Ealert(1)%3C/script%3E") form-sqli=$(c -d "user=admin' OR '1'='1' --" http://127.0.0.1:$P/login)"
done
