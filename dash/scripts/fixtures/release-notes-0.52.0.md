## Before you update

- One-click update is off for this release. An admin applies it under **Settings › Updates**, or with `install.sh`.
- Run the connect command once on every cluster except the one Pyo runs on. It's under **Refresh this cluster** in the cluster's settings. Envoy Gateway's chart now comes from Pyo's mirror, and only the connect command installs it. The agent's roles and admission policy changed too.
- The server Pyo runs on refreshes its own cluster by itself a few minutes after the update. If that's your only cluster, there's nothing to run.
- Pyo's own Postgres, ClickHouse, backup store and registry restart once during the update. Their images moved to Pyo's mirror. Pyo is down for a short time while they start.
- Every Postgres and KV database restarts once when its cluster takes the update. A database with replicas switches over to a replica first.
- Pyo Gateway's proxies restart once on each server. A server's new proxy is ready before its old one stops. Open WebSockets and streams reconnect.
- The Cloudflare Tunnel connector restarts once on each cluster that uses it.
- The firewall (WAF) restarts once on each server where a service uses it.
- Each service's open connections, like WebSockets and streams, drop once the next time Pyo applies its routes. Plain HTTP and the tailnet drop once too. After that, firewall changes no longer drop them.
- Country rules on domains behind Cloudflare or through Cloudflare Tunnel now use the country Cloudflare sends. Keep Cloudflare's IP geolocation on (it is by default). If it's off, "is not in" rules there block everyone.
- Self-hosted sandbox servers stop taking commands after the update until an admin presses **Update** on each one and runs the command it shows on that server. Their sandboxes keep running.
- HTTP/3 is off until you turn it on in a cluster's **Gateway** settings. It needs UDP port 443 open to every server, in your provider's firewall too.
- Node.js apps that don't pin a Node.js version build on Node.js 26 instead of 24 from their next deploy (Railpack v0.43.1). To stay on 24, set `engines.node` in `package.json`.

### 🚀 Features

- [`c107994`](https://github.com/upsetdev/pyo/commit/c107994a04b648d540e0ffdae02effb2ac3c18cc) feat: route paths of one hostname to different services of a team, like `/api` to one and `/` to another. The longest path wins, and each service keeps its own firewall and WAF settings. Paths work on Tailscale domains and in Compose files too
- [`0756c5c`](https://github.com/upsetdev/pyo/commit/0756c5c9687a02245bc219ade5c612b96625b9ca) feat: add a domain on a path with `pyo domain add app.example.com/api`, and `--strip-prefix` to send `/api/x` to the service as `/x`
- [`1e7308e`](https://github.com/upsetdev/pyo/commit/1e7308e720a7c6c1591e606549e1b7fdf6e49958) feat: add, edit and remove path routes on a domain's page. Edit can move a path to another service on the same cluster
- [`8d3aa26`](https://github.com/upsetdev/pyo/commit/8d3aa26ef82a71421ecf16b440a017ee1baa1c37) feat: count a shared hostname's requests per service. Each service's analytics and request log show only its own
- [`056f97d`](https://github.com/upsetdev/pyo/commit/056f97d5c139a9f959d265e609e18a71c7f8a896) feat: add redirects and header rules to a domain. A domain without a port only redirects, like www to the apex. MCP gets `set_domain_redirects`, `set_domain_headers` and `redirect_to` on `add_domain`
- [`a4ccfeb`](https://github.com/upsetdev/pyo/commit/a4ccfeb1ef3b80b8cb71e7ee127dad7db09404b2) feat: add `pyo domain redirect` and `pyo domain header` to the CLI
- [`8a44238`](https://github.com/upsetdev/pyo/commit/8a44238491d017844f819d6f62327ec7de648ce3) feat: list the team's domains with one row per hostname. A hostname's page shows its routes, redirects, headers, DNS and HTTPS
- [`ba70f21`](https://github.com/upsetdev/pyo/commit/ba70f211271a3f9dc6112f1ef8b0b6ab7fba4c8c) feat: show each domain's requests over the last 24 hours on the Domains list, with a small chart. The API has it as `GET /observability/domains` and MCP as `get_domain_traffic`
- [`12d0ec5`](https://github.com/upsetdev/pyo/commit/12d0ec59f63c28e777021db3df4b80ec64a5c743) feat: show a green lock before a hostname when HTTPS works, on the Domains list and a service's Domains tab. A domain that needs attention shows an icon and a few words instead. The Status column and the TLS badge are gone
- [`913d525`](https://github.com/upsetdev/pyo/commit/913d525bd1b0977cffb4107a4fe5781b4c064052) feat: remove one domain through the API or the MCP tool `remove_domain`, without saving the whole service. It deletes the DNS record Pyo made, or puts back the records a tunnel replaced
- [`8f37008`](https://github.com/upsetdev/pyo/commit/8f370083dc8290081246fd6de6781ace063ce80f) feat: add sandbox servers on Pyo Cloud. A team with Tailscale gets a one-time join code and one command. The server joins over HTTPS and opens its own connection to Pyo for its work. Pyo never calls it. `pyo sandbox servers add` asks for a join code too
- [`7a7dfc1`](https://github.com/upsetdev/pyo/commit/7a7dfc114a13c1d50a0b6206a65aa820fb7eea66) feat: self-hosted sandbox servers join over HTTPS and open their own connection to Pyo, as on Pyo Cloud
- [`b5b97f1`](https://github.com/upsetdev/pyo/commit/b5b97f17cfbf841e86c7f2893bd22af00e456cef) feat!: stop calling sandbox servers directly. A server that joined before 0.52.0 takes no commands until you press **Update** on it and run the command it shows. Its sandboxes keep running
- [`a7cdf21`](https://github.com/upsetdev/pyo/commit/a7cdf21ae97e893107f2c1c790531a9300b9f679) feat!: remove `--pyo` from `sandbox.sh`. A command with it says to press **Update** in Pyo
- [`28639b1`](https://github.com/upsetdev/pyo/commit/28639b1d13dc967bcf52413901d216d5673ad0ce) feat: show **Needs update** on sandbox servers and their sandboxes in the dashboard, and **needs update** in `pyo sandbox servers ls`
- [`ee602f1`](https://github.com/upsetdev/pyo/commit/ee602f13c984aa40643505f25cd927ff140499a3) feat: turn a cluster's traffic paths on and off one by one: public ports, Cloudflare Tunnel and Tailscale. A cluster keeps at least one on. Each domain takes one path, and a domain whose path is off shows **Not served**
- [`69de329`](https://github.com/upsetdev/pyo/commit/69de329885c9a0e082c6555d68ca9f6eea54d207) feat: show the traffic paths as switches in a cluster's **Traffic** settings. Turning off a path that domains use lists them first
- [`49a807d`](https://github.com/upsetdev/pyo/commit/49a807db4ec016cfea0174b1d87b810c43e118c6) feat: add `pyo cluster paths` to show and change a cluster's traffic paths
- [`7907aab`](https://github.com/upsetdev/pyo/commit/7907aab97799d11f58e801e35fc762420781e140) feat: keep websites off the servers' Tailscale addresses while a cluster's Tailscale path is off. Servers stay on the tailnet
- [`8536681`](https://github.com/upsetdev/pyo/commit/8536681543b7d1b9cd5e4ca302a73149057bc28d) feat: add a **Gateway** panel to cluster settings. It shows the controller, each server's proxy and the firewall, and sets HTTP/3, compression, the TLS minimum and the default request and idle timeouts
- [`a588129`](https://github.com/upsetdev/pyo/commit/a5881291398726d4df22b0363521f5d32ee68a12) feat: move the PROXY protocol switch to a cluster's **Gateway** settings, next to HTTP/3 and the TLS minimum. It needs public ports on. The API is unchanged
- [`af8fc25`](https://github.com/upsetdev/pyo/commit/af8fc25a6c70072816879a95974e914e4f00a22b) feat: add `pyo cluster gateway` to show and change those settings. HTTP/3 and the TLS minimum drop open connections once and ask for `--yes` first
- [`61515a9`](https://github.com/upsetdev/pyo/commit/61515a99dd75c01f53f9b0ae297e06b1db8ad8ac) feat: let each team choose its observability within the installation's limits: on or off, request logs, IP addresses and query strings. The admin sets a default and a limit for every team
- [`5fabe3a`](https://github.com/upsetdev/pyo/commit/5fabe3aa80aa8d3971aa65e503e1c4f71890cfde) feat: add an **Observability** tab to team settings, and the defaults and limits to the installation's **Settings › Observability**
- [`3420694`](https://github.com/upsetdev/pyo/commit/3420694f1bfb0b759918846806e64feabeb788ef) feat: add `--team`, `--request-logs`, `--ip` and `--query-strings` to `pyo observability`
- [`462de29`](https://github.com/upsetdev/pyo/commit/462de292f0d18b71e08858ed3546e86418513a56) feat: keep only query string names, or no query string, when the team says so. With request logs off, servers keep no requests and still count totals
- [`f1cccea`](https://github.com/upsetdev/pyo/commit/f1cccea1bc4eac960b70f1b964c0a76a55c7fa51) feat: add a **Paths** tab to analytics with every requested path and its request count, up to 100

### 🐞 Bug Fixes

- [`ea8ff67`](https://github.com/upsetdev/pyo/commit/ea8ff670b06a0d238d7b62c389d3b8a140cb4952) fix: pull third-party images and Envoy Gateway's chart from Pyo's mirror at `ghcr.io/upsetdev/pyo-mirror` instead of Docker Hub. Docker Hub limits anonymous pulls, and a fresh install failed on it
- [`9c06f7a`](https://github.com/upsetdev/pyo/commit/9c06f7ad0341e424c4d7a4b622c2a543eb71809b) fix: never take over a namespace Pyo didn't create. A project named like a site already on the server could replace that site. The deploy now stops with the reason
- [`8723627`](https://github.com/upsetdev/pyo/commit/8723627b1f34fd003b1fe5ae2a5fbb5fb06924ad) fix: stop connecting a server that runs cert-manager or Envoy Gateway that Pyo didn't install. Pyo used to replace them
- [`f8ca3ab`](https://github.com/upsetdev/pyo/commit/f8ca3ab76f2d645bb0c9fcfbd2a4718aac349c05) fix: stop the first database on a cluster that already runs a Postgres or MySQL operator Pyo didn't install. Pyo used to take it over
- [`b83e807`](https://github.com/upsetdev/pyo/commit/b83e807768bec12db67abeb06be8923f95d359dd) fix: keep a saved value when an assistant sends back the hidden `•••` it read. The mask was saved as the value
- [`981a6c5`](https://github.com/upsetdev/pyo/commit/981a6c53db2193a49a0e6089264129f6c8280049) fix: label a service's image by its registry. Images from ghcr.io and other registries added through MCP or a Compose import were saved as public Docker Hub images
- [`248f7b9`](https://github.com/upsetdev/pyo/commit/248f7b9202a34ca7cf5a373355d1ea359880dbbd) fix: delete a service whose first deploy was cancelled. A service now shows Deleting until it's gone, and a failed delete shows its reason
- [`b3c3e4b`](https://github.com/upsetdev/pyo/commit/b3c3e4b65de7ad8842112075cbb02b5f805de3a4) fix: keep a shared hostname's DNS record when one of its paths is removed. The other paths lost DNS
- [`4f66777`](https://github.com/upsetdev/pyo/commit/4f66777ca4f21d858970099f9d6d7ed5a474d12f) fix: show a new tunnel as starting until Cloudflare first reports. It said Pyo can't reach Cloudflare
- [`be7b99b`](https://github.com/upsetdev/pyo/commit/be7b99bec841092faf12651128e8578eaf532487) fix: show at once when a DNS record is in the way of moving a domain to the tunnel. It took about a minute
- [`c6e90ab`](https://github.com/upsetdev/pyo/commit/c6e90abd29c7474ac436856cffd5add9b8ea4b6a) fix: check a tunnel domain against Cloudflare's addresses, not the cluster's servers. It shows "Moving to the tunnel…" until it's live
- [`6e388ce`](https://github.com/upsetdev/pyo/commit/6e388cefb42ffd596ae9af5363d8ac0574eecf26) fix: clear an old DNS error once a domain is live through the tunnel
- [`fe35ff3`](https://github.com/upsetdev/pyo/commit/fe35ff3aa99b9fcf9b64e9616c5048e3432b7317) fix: move Switch back from Tunnel into the domain's menu on a service's Domains tab. The Cloudflare logo's tooltip shows the record
- [`b20757d`](https://github.com/upsetdev/pyo/commit/b20757dcc735aad661cb877ed1c29c0ec1066d89) fix: delete the TXT records Pyo made to prove a domain once the proof is done, or when the domain or its service goes
- [`17e51f8`](https://github.com/upsetdev/pyo/commit/17e51f819dd2b774d105763a26a668b11394f3c0) fix: prove domains on Cloudflare zones from servers without IPv6. Every proof failed with "no connections available"
- [`b5de927`](https://github.com/upsetdev/pyo/commit/b5de92701fe137cea96d1f145457b878c37b6bba) fix: say in plain words why a system workload or the tunnel connector can't run: it can't download its image, a setting is missing, or it keeps stopping
- [`ade18cb`](https://github.com/upsetdev/pyo/commit/ade18cb751b3089bca4a60af3e4947e5f425bc4a) fix: say what holds a name that's taken on the tailnet: a device, another of the team's clusters, or something else
- [`6eae640`](https://github.com/upsetdev/pyo/commit/6eae640e8a9051954d4c7aea0f0af0a110753e1f) fix: keep a server's own Tailscale tags when Pyo adds `tag:pyo`
- [`92de961`](https://github.com/upsetdev/pyo/commit/92de961db16d2f4d0bff1ca47f5dcdb18e4c5b70) fix: show a cluster's tailnet status in its Traffic card
- [`70de19c`](https://github.com/upsetdev/pyo/commit/70de19c9dc51a91058fe756bb41247fa69a7224b) fix: open sandbox terminals and files on a self-hosted Pyo. The browser blocked them
- [`cd93adb`](https://github.com/upsetdev/pyo/commit/cd93adb6b018ae6a70b70f0e526ccdfa67635f05) fix: pass on a sandbox server's own reason when it refuses a command
- [`f91eb4a`](https://github.com/upsetdev/pyo/commit/f91eb4ac0c0caf0a2a9b8416aae00526281f9c1e) fix: grow the request log storage when a team keeps 5 or 10 GB. It stayed at 2 GiB
- [`9ec313d`](https://github.com/upsetdev/pyo/commit/9ec313d89717b0085776fe6f292c115011c0f199) fix: say which minutes the request summary counts. The dashboard shows "Up to 14:05" under the request count
- [`4f49921`](https://github.com/upsetdev/pyo/commit/4f49921c7e844a9be16a54380c31a31fd9b2f37b) fix: find a service's requests by its name or id, and a domain's by its hostname. A filter that matches nothing of the team's is refused instead of returning an empty log
- [`8e27794`](https://github.com/upsetdev/pyo/commit/8e27794c0dffed6ded53c8ecddac5b5628fe8e8c) fix: say full IP addresses stay 31 days at most. Settings said 180 days
- [`51be889`](https://github.com/upsetdev/pyo/commit/51be8892cf32bd9e27a735ffc9ce0be91a033469) fix: show the installation settings to installation admins only
- [`5e7b0e6`](https://github.com/upsetdev/pyo/commit/5e7b0e64b7c42b0bde21a01a7ebbe069b1edf7f7) fix: show team settings read only, without Save, to members who can't change them
- [`526a0fd`](https://github.com/upsetdev/pyo/commit/526a0fddebd7196f4b767fb54dc002626de13b9d) fix: show Delete team to team owners only
- [`befcc0f`](https://github.com/upsetdev/pyo/commit/befcc0f2074488cd44aef3b0bd163417c0970b48) fix: remove the unused deployment preferences from team settings. Auto-deploy stays in each service's Git settings
- [`4a0f166`](https://github.com/upsetdev/pyo/commit/4a0f1665b7b7207d426ad73bf3e1c5770187c8c3) fix: load the dashboard for admins when a cluster never reported. The page failed to load
- [`187ad76`](https://github.com/upsetdev/pyo/commit/187ad768491d148256d02ddeda6c6fd9e3403ce7) fix: open a deployment's details in its service's Deployments tab. Old links and alerts forward there
- [`3711dce`](https://github.com/upsetdev/pyo/commit/3711dcee6a35ef2f73062cf5c902d226ca45d487) fix: shorten long image digests in deployment headers. The full reference is in a tooltip with a copy button
- [`662a378`](https://github.com/upsetdev/pyo/commit/662a378ba6cef68471b516817c48b4685a61248e) fix: a Pyo run for many teams refuses to start without its isolation setting or with local build images. Self-hosted Pyo is unchanged
- [`3dadc8a`](https://github.com/upsetdev/pyo/commit/3dadc8ad1c6826d7f580be0fbe6cd85693bc81eb) fix: keep the image mirror outside Pyo's own image path, so installed Pyos accept the release. A CI check builds the release and runs that rule
- [`1e82af5`](https://github.com/upsetdev/pyo/commit/1e82af515a67e90e9428a491c596d864523414f0) fix: open WebSockets through the WAF in every mode. The WAF judges the upgrade request's headers
- [`1f4c766`](https://github.com/upsetdev/pyo/commit/1f4c7664f4a92ed82627ae15ee787f01c98fab1a) fix: refuse an upgrade request that declares a body when the WAF is on
- [`22d8799`](https://github.com/upsetdev/pyo/commit/22d8799ce83ddcc78501554f4a85593e0376929e) fix: keep firewall changes from dropping open connections. A service's first or last address, country or rate limit rule used to drop every open connection to its domains and to all plain HTTP
- [`2d4dd58`](https://github.com/upsetdev/pyo/commit/2d4dd58925b86890ea7ae5bafa4866155066baeb) fix: keep a service's first or last country rule from dropping open connections. The country lookup now runs on every request, about 2 µs of proxy CPU each
- [`31e2e19`](https://github.com/upsetdev/pyo/commit/31e2e192a4d7db65e39c0dbf5daf7d69a755e02e) fix: keep open connections when a header rule or a path gives a domain its own route
- [`6b0ff78`](https://github.com/upsetdev/pyo/commit/6b0ff78126e6b05d28d127c232689956eb0fc705) fix: stop dropping plain HTTP connections when a service is added or removed. A visitor the firewall refuses over plain HTTP now gets the proxy's own 403 text. HTTPS keeps the firewall's message
- [`2cd4b32`](https://github.com/upsetdev/pyo/commit/2cd4b32cbd556d01a1eec42542a1956e0b4cb5bc) fix: send every running service's routes again once a cluster's install is current, so what an older install refused goes in then
- [`9d7241c`](https://github.com/upsetdev/pyo/commit/9d7241cfeedec2ab60ecdd63dad9f6f05d78ed6e) fix: restart Pyo Gateway's proxies once on an update from 0.51.0, not twice
- [`8ea6652`](https://github.com/upsetdev/pyo/commit/8ea6652ddd9d907efa198d253a0362fe1b2b0ac1) fix: tag the `geoip` image with its database's date and the `waf` image with a hash of its sources, and reuse them when they didn't change. The proxies and the WAF no longer restart with every release
- [`7667e56`](https://github.com/upsetdev/pyo/commit/7667e5653273279aa958ef90aa7b389723f85170) fix: restart k3s in the connect command only when the registry login changed. Every refresh used to restart it
- [`514c089`](https://github.com/upsetdev/pyo/commit/514c08947c87bd95bf5cc8852e3a987379b3111d) fix: trust a new tunnel connector before cloudflared starts. A new connector used to answer 403 for up to 12 seconds
- [`eb720ae`](https://github.com/upsetdev/pyo/commit/eb720aed0d9cd719a57b12844ce8d0f26e7c8292) fix: let requests finish when a tunnel connector stops
- [`4e5367b`](https://github.com/upsetdev/pyo/commit/4e5367b1c5d53082de1dd4a24207f4e09ade50ab) fix: keep the tunnel gate's image when only the agent's image changed, so the connector doesn't restart for it
- [`8f850b6`](https://github.com/upsetdev/pyo/commit/8f850b66337e0904d2207567785cfca1c69c3027) fix: start cloudflared with a local config, so a new connector serves before Cloudflare sends its routes. It answered 503 for a moment
- [`1c3a7a0`](https://github.com/upsetdev/pyo/commit/1c3a7a0ea03f6e41a74d54087de16616402ccbbe) fix: keep tunnel connectors up while rotating the token. Visitors got Cloudflare's 530 for about 5 seconds. The tunnel shows `rotating` until the new connectors are up
- [`c01e733`](https://github.com/upsetdev/pyo/commit/c01e733e4d7c1a832736c07bc9c9e71f53b00926) fix: revoke the tunnel's previous certificate when a new one is issued within a day of the last. It stayed valid
- [`f09f65d`](https://github.com/upsetdev/pyo/commit/f09f65d007d1776cc1ed56505d173e6aeb7d4687) fix: keep a moved domain on both paths until Cloudflare serves its new record
- [`5b9c020`](https://github.com/upsetdev/pyo/commit/5b9c0203fd5ff0da6cf94949c112a884aa213599) fix: warn that turning the Tailscale path off or on drops open connections to Pyo on the tailnet, on the cluster Pyo runs on, until its connect command has run for 0.52.0
- [`3b31b8e`](https://github.com/upsetdev/pyo/commit/3b31b8e13928897fa68b5b0102e7d1e7929f38d5) fix: say that TCP and UDP ports pause when public ports change
- [`842968d`](https://github.com/upsetdev/pyo/commit/842968da8614fde06054ebed4d0438778d388032) fix: stop counting redirects as page views
- [`e039057`](https://github.com/upsetdev/pyo/commit/e039057fe407d7936df754e19ab113bae60a0417) fix: save a lowered observability limit when a team is deleted at the same time. The whole save failed
- [`1cee6e2`](https://github.com/upsetdev/pyo/commit/1cee6e21ba6e68d4168fd40f67241630461f15a0) fix: compare a release candidate as its release. A candidate Pyo queued no agent updates, and its sandbox base template could replace a newer Pyo's choice
- [`4799d78`](https://github.com/upsetdev/pyo/commit/4799d7851486629cde44f65afbdfeac3d954195b) fix: upgrade a release candidate's CLI to its release. `pyo upgrade` said it was up to date
- [`ba4ab39`](https://github.com/upsetdev/pyo/commit/ba4ab39ed4ef388d2d940aaec5ea78e50c030723) fix: make firewall country rules work on domains behind Cloudflare and through Cloudflare Tunnel. They use Cloudflare's country header there. A country rule on such a domain made it answer 500
- [`8695a56`](https://github.com/upsetdev/pyo/commit/8695a56a2b0bb788bb0636232a7370d2b12d7ca2) fix: keep a domain's HTTPS certificate in place while it moves to Cloudflare Tunnel. New connections failed for 2 to 4 seconds
- [`24f949b`](https://github.com/upsetdev/pyo/commit/24f949bca859e9242a200840d6803f75bffd9a55) fix: remove a moving domain's old route only once the proxies take the new one. The domain answered 404 for about a second
- [`33bf182`](https://github.com/upsetdev/pyo/commit/33bf182095cfc3056476b1b54687301e6eb0a083) fix: wait 3 seconds after the new route is accepted before removing the old one. One request still got a 404
- [`a880fda`](https://github.com/upsetdev/pyo/commit/a880fda22516540a1254844e7e466f881e98ad52) fix: a domain through Cloudflare Tunnel with the firewall on no longer answers 403 for a moment while a new tunnel connector starts during an update
- [`4b5d711`](https://github.com/upsetdev/pyo/commit/4b5d711ca0d81e3d78d8f801f508ab5b4f9e83ac) fix: let a new tunnel connector into the tunnel policies 0.51.0 wrote. On the update from 0.51.0 its address was refused, and the domain answered 403 until its routes were written again
- [`e8c66c6`](https://github.com/upsetdev/pyo/commit/e8c66c65c46b4c40c8c5e9156b26ad88d90ea545) fix: keep a cluster's Settings tab from changing shape while it loads
- [`3452a62`](https://github.com/upsetdev/pyo/commit/3452a62631080eef12a11f4d7d6b5913b777f78b) fix: show the Tailscale switch off until the team connects Tailscale
- [`391a24b`](https://github.com/upsetdev/pyo/commit/391a24b20ff5c1d58ad216bc0c50a6cc62457086) fix: show distinct chart labels when a chart has no data. They read 0, 0, 1, 1, 1

### ⬆️ Security Fixes

- [`e1bd94d`](https://github.com/upsetdev/pyo/commit/e1bd94d9a190c1a9a1e133dc364d6eca980d88c9) fix(security): stop clients from claiming a country. A client whose address the GeoIP database can't place could send Envoy Gateway's internal country header and get past a country block or an "only these countries" rule. Pyo Gateway now removes those headers from every request as it comes in
- [`f739156`](https://github.com/upsetdev/pyo/commit/f739156ba29b38b25f314f39c59cfbd1ac7b6257) fix: update Railpack to v0.43.1. v0.40.1 was built with Go modules that have six fixed CVEs
- [`4d3dade`](https://github.com/upsetdev/pyo/commit/4d3dade6283e03e137693904a8f805d08562d20f) fix(security): pin the GeoLite2 databases by commit and checksum. The build checks both
- [`dcc05b9`](https://github.com/upsetdev/pyo/commit/dcc05b9e1bdeb589c2ffe6be2218b8991dd3cdb0) build: pin the WAF image's base images by digest

### 📝 Docs

- [`1d68bd4`](https://github.com/upsetdev/pyo/commit/1d68bd4d1d0a1303bde265e320d0bcddfdcff874) docs: say what command signing protects and what it doesn't
- [`659e644`](https://github.com/upsetdev/pyo/commit/659e644e039b7f2f3ef9d8bb2dc5fd3825a8f332) docs: describe the mirror of third-party images in the release guide
- [`1ce7180`](https://github.com/upsetdev/pyo/commit/1ce71801c54596e885419f98322fb36a3f4da410) docs: add the design notes for path routes, redirects and header rules
- [`4ab66d2`](https://github.com/upsetdev/pyo/commit/4ab66d28ce5a03beaa8d4c03c45938ffd872a736) docs: add the design notes for sandbox servers on Pyo Cloud and on the new connection
- [`147587f`](https://github.com/upsetdev/pyo/commit/147587f328c6308b7e26699bb383ec2c697aacc6) docs: describe the tunnel's trust watch and start gate
- [`90c13f1`](https://github.com/upsetdev/pyo/commit/90c13f1e32725f77fea8a4d073eb6be27d98ffcb) docs: describe the image tags and the inbound pods record in the release guide
- [`2314f58`](https://github.com/upsetdev/pyo/commit/2314f582cc09e47af175419a26bb2ed003a674f1) docs: add the design notes for traffic paths and the gateway section
- [`9616009`](https://github.com/upsetdev/pyo/commit/9616009a36c21055817fb986b8644e6e79211d78) docs: record the removal of the direct sandbox path
- [`0cbd3b8`](https://github.com/upsetdev/pyo/commit/0cbd3b880af70cdde6311528fb12e84e3c3dbeb4) docs: record how country rules work behind Cloudflare and the claimed country fix
- [`df17457`](https://github.com/upsetdev/pyo/commit/df17457f21ac3cf4b628153856419be3d4f22984) docs: record how routes are replaced before the old ones go

**Full Changelog**: https://github.com/upsetdev/pyo/compare/v0.51.0...v0.52.0
