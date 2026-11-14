import { Panel } from "~/components/ui/Panel";
import { CopyButton } from "~/components/ui/Table";

/** How to get the `pyo` command line and sign it in to this Pyo. */
export function CliInstallPanel() {
  const origin = typeof window === "undefined" ? "" : window.location.origin;
  const steps = [
    "curl -fsSL https://pyo.dev/cli.sh | sh",
    `pyo login --url ${origin}`,
    "pyo link && pyo deploy",
  ];
  return (
    <Panel title="CLI">
      <ul className="list-rows">
        {steps.map((c) => (
          <li key={c} className="flex items-center gap-2 px-4 py-2.5">
            <code className="min-w-0 flex-1 truncate font-mono text-[12px]">
              {c}
            </code>
            <CopyButton value={c} label="Copy command" />
          </li>
        ))}
      </ul>
    </Panel>
  );
}
