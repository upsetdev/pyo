import { useState } from "react";
import { AppLink } from "~/components/ui/AppLink";
import { Button } from "~/components/ui/Button";
import { Field, Input, Select } from "~/components/ui/Field";
import { Highlighted } from "~/components/ui/Highlighted";
import { Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";
import { CODE_CLASS } from "~/lib/pyo/highlight";
import { api, servicePath, usePyo } from "~/lib/pyo/store";
import { toast } from "~/lib/pyo/toast";
import { useDatabase } from "./DatabaseLayout";

const MASK = "••••••••";

export function DatabaseConnect() {
  const d = useDatabase();
  const s = usePyo();
  const [serviceId, setServiceId] = useState("");
  const [prefix, setPrefix] = useState("");
  const [busy, setBusy] = useState(false);
  const [secret, setSecret] = useState<{ password: string; token: string | null; readonlyToken?: string | null } | null>(null);
  if (!d) return null;
  const me = s.members.find((m) => m.teamId === d.teamId && m.email === s.user.email);
  const admin = !me || me.role === "owner" || me.role === "admin";
  const connected = new Set(d.connections.map((c) => c.serviceId));
  // Services on its cluster: they reach it inside the cluster.
  const candidates = s.services.filter((a) => a.clusterId === d.clusterId && !connected.has(a.id) && s.projects.some((p) => p.id === a.projectId && p.teamId === d.teamId));
  const chosen = serviceId || candidates[0]?.id || "";
  const app = (id: string) => s.services.find((a) => a.id === id);
  // Masked values, or with the password once revealed.
  const show = (v: string) => (secret ? v.replaceAll(MASK, secret.password) : v);
  const shown = d.variables.map((v) => ({
    key: v.key,
    value:
      v.key === "KV_REST_API_TOKEN" ? (secret?.token ?? MASK)
      : v.key === "KV_REST_API_READ_ONLY_TOKEN" ? (secret?.readonlyToken ?? MASK)
      : show(v.value),
  }));
  return (
    <div className="space-y-6">
      <Panel title="Connect to a service" description="Adds its connection variables to the service. They apply on its next deploy.">
        <div className="space-y-4 p-4">
          {candidates.length ? (
            <div className="flex flex-wrap items-end gap-3">
              <Field label="Service" className="min-w-[220px] flex-1">
                {(p) => (
                  <Select {...p} value={chosen} onChange={(e) => setServiceId(e.target.value)}>
                    {candidates.map((a) => (
                      <option key={a.id} value={a.id}>
                        {s.projects.find((p2) => p2.id === a.projectId)?.name} / {a.name}
                      </option>
                    ))}
                  </Select>
                )}
              </Field>
              <Field label="Prefix" optional className="w-[140px]">
                {(p) => (
                  <Input
                    {...p}
                    mono
                    value={prefix}
                    placeholder={d.engine === "postgres" ? "POSTGRES" : d.engine === "mysql" ? "MYSQL" : "KV"}
                    onChange={(e) => setPrefix(e.target.value.toUpperCase())}
                  />
                )}
              </Field>
              <Button
                size="sm"
                disabled={!chosen || busy}
                onClick={async () => {
                  setBusy(true);
                  const r = await api.connectDatabase(d.id, chosen, prefix);
                  setBusy(false);
                  if ("error" in r) toast({ tone: "error", title: "Not connected", description: r.error });
                  else {
                    setPrefix("");
                    toast({ title: `Connected to ${app(chosen)?.name}`, description: `${r.data.variables.join(", ")}. Redeploy to apply.` });
                  }
                }}
              >
                Connect
              </Button>
            </div>
          ) : (
            <p className="text-[12.5px] text-muted-foreground">No other service of the team runs on this database's cluster.</p>
          )}
          {d.connections.length ? (
            <ul className="list-rows rounded-lg border border-border">
              {d.connections.map((c) => (
                <li key={c.serviceId} className="flex flex-wrap items-center justify-between gap-3 px-3 py-2 text-[12.5px]">
                  <span className="min-w-0">
                    <AppLink href={servicePath(c.serviceId)} className="font-medium hover:text-primary">
                      {app(c.serviceId)?.name ?? "Deleted service"}
                    </AppLink>{" "}
                    <span className="text-muted-foreground">· {c.variables.join(", ")}</span>
                  </span>
                  <Button
                    size="xs"
                    variant="ghost"
                    onClick={async () => {
                      const r = await api.disconnectDatabase(d.id, c.serviceId);
                      if ("error" in r) toast({ tone: "error", title: "Not disconnected", description: r.error });
                      else toast({ title: "Disconnected", description: "Redeploy the service to apply." });
                    }}
                  >
                    Disconnect
                  </Button>
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      </Panel>
      <Panel
        title="Connection details"
        description={
          d.engine === "postgres" && d.pooler
            ? "Inside the cluster only. DATABASE_URL goes through the pooler. Use DATABASE_URL_DIRECT for migrations, LISTEN/NOTIFY and advisory locks."
            : "Inside the cluster only."
        }
        actions={
          admin && !secret ? (
            <Button
              size="xs"
              variant="secondary"
              onClick={async () => {
                const r = await api.revealDatabasePassword(d.id);
                if ("error" in r) toast({ tone: "error", title: "Couldn't show it", description: r.error });
                else setSecret({ password: r.data.password, token: r.data.token, readonlyToken: r.data.readonlyToken });
              }}
            >
              Show password
            </Button>
          ) : undefined
        }
      >
        <ul className="list-rows">
          <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3 text-[12.5px]">
            <span className="text-muted-foreground">User</span>
            <code className="font-mono">{d.username}</code>
          </li>
          {d.database ? (
            <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3 text-[12.5px]">
              <span className="text-muted-foreground">Database</span>
              <code className="font-mono">{d.database}</code>
            </li>
          ) : null}
          <li className="flex flex-wrap items-center justify-between gap-3 px-4 py-3 text-[12.5px]">
            <span className="text-muted-foreground">Password</span>
            <span className="flex items-center gap-1">
              <code className="font-mono">{secret?.password ?? MASK}</code>
              {secret ? <CopyButton value={secret.password} label="Copy password" /> : null}
            </span>
          </li>
          {shown.map((v) => (
            <li key={v.key} className="space-y-1 px-4 py-3 text-[12.5px]">
              <p className="text-muted-foreground">{v.key}</p>
              <div className="flex items-start gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2">
                <code className={`min-w-0 flex-1 break-all text-[12px] ${CODE_CLASS}`}>
                  <Highlighted text={v.value} lang="value" />
                </code>
                {!v.value.includes(MASK) ? <CopyButton value={v.value} label={`Copy ${v.key}`} /> : null}
              </div>
            </li>
          ))}
        </ul>
      </Panel>
    </div>
  );
}
