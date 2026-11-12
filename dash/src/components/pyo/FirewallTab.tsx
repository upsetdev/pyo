import { FirewallEditor } from "~/components/pyo/Firewall";
import { FirewallAnalytics } from "~/components/pyo/FirewallAnalytics";
import { WafSection } from "~/components/pyo/Waf";
import type { Application, Cluster } from "~/lib/pyo/types";

/**
 * A service's Firewall tab: who may connect (access rules), its web
 * application firewall (WAF), and what both did.
 */
export function FirewallTab({ cluster, app }: { cluster: Cluster; app: Application }) {
  return (
    <div className="space-y-6">
      <FirewallEditor cluster={cluster} app={app} />
      <WafSection app={app} />
      <FirewallAnalytics app={app} />
    </div>
  );
}
