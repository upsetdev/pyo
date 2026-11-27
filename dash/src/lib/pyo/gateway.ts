// The Gateway section's settings form (components/pyo/Gateway.tsx): what
// it saves, what it says before a save that drops connections, and when the
// cluster's agent can't take the settings yet. The API checks the same
// (gateway_settings.rs) in the same words.

import type { Cluster, GatewaySettings } from "./types";

/** Today's gateway, for a cluster whose API predates the section. */
export const GATEWAY_DEFAULTS: GatewaySettings = {
  http3: false,
  compression: false,
  tlsMin: "1.2",
  requestTimeout: 300,
  idleTimeout: 3600,
};

export const REQUEST_RANGE = "The request timeout is 1 to 3600 seconds, or 0 for no limit.";
export const IDLE_RANGE = "The idle timeout is 1 to 1440 minutes.";
export const IDLE_SHORT = "The idle timeout must be at least the request timeout.";

/** The form's values: the idle timeout in minutes, as typed. */
export type GatewayForm = {
  http3: boolean;
  compression: boolean;
  tlsMin: "1.2" | "1.3";
  requestTimeout: string;
  idleMinutes: string;
};

export function formOf(s: GatewaySettings): GatewayForm {
  return {
    http3: s.http3,
    compression: s.compression,
    tlsMin: s.tlsMin,
    requestTimeout: String(s.requestTimeout),
    idleMinutes: String(Math.round(s.idleTimeout / 60)),
  };
}

/** What's wrong with the form, in the API's words, or the settings it saves. */
export function checkForm(f: GatewayForm): { error: string } | { settings: GatewaySettings } {
  const whole = (v: string) => (/^\d+$/.test(v.trim()) ? Number(v.trim()) : NaN);
  const request = whole(f.requestTimeout);
  if (!Number.isInteger(request) || request > 3600) return { error: REQUEST_RANGE };
  const minutes = whole(f.idleMinutes);
  if (!Number.isInteger(minutes) || minutes < 1 || minutes > 1440) return { error: IDLE_RANGE };
  const idle = minutes * 60;
  if (request > 0 && idle < request) return { error: IDLE_SHORT };
  return {
    settings: { http3: f.http3, compression: f.compression, tlsMin: f.tlsMin, requestTimeout: request, idleTimeout: idle },
  };
}

/** The fields that differ, as the API takes them. */
export function changes(from: GatewaySettings, to: GatewaySettings): Partial<GatewaySettings> {
  const out: Partial<GatewaySettings> = {};
  for (const k of Object.keys(to) as (keyof GatewaySettings)[]) {
    if (from[k] !== to[k]) (out as Record<string, unknown>)[k] = to[k];
  }
  return out;
}

/** What the save's confirm says, for the changes that drop connections. Empty: no confirm. */
export function dropLines(name: string, from: GatewaySettings, to: GatewaySettings): string[] {
  const lines: string[] = [];
  if (from.http3 !== to.http3)
    lines.push("Open connections drop once, one server at a time.");
  if (from.tlsMin !== to.tlsMin) lines.push(`Open HTTPS connections to every website on ${name} drop once.`);
  if (!lines.length) return lines;
  lines.push("Visitors reconnect. WebSockets and uploads in progress are cut.");
  if (to.http3 && !from.http3) lines.push("Open UDP port 443 to every server in your provider's firewall.");
  return lines;
}

/** Why the agent can't take the settings yet, or null. */
export function agentBlock(c: Cluster): string | null {
  if (!c.capabilities.gatewaySettings) return "Update this cluster's agent first.";
  const state = c.capabilities.agentInstall?.state;
  if (state === "updating") return "This cluster is updating. Try again in a few minutes.";
  if (state === "outdated" || state === "roles") return "Run the command under Refresh this cluster first.";
  return null;
}
