import { KeyIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { Button } from "~/components/ui/Button";
import { CopyButton } from "~/components/ui/Table";
import { api } from "~/lib/pyo/store";

// How to restore a team's backups: its restic repository, the password
// (revealed by owners and admins, logged), and the commands.

export function RestoreSteps({
  teamId,
  repository,
  admin,
  volumeId,
}: {
  teamId: string;
  repository: string;
  admin: boolean;
  /** Just this volume's snapshots. */
  volumeId?: string;
}) {
  const [password, setPassword] = useState<string | null>(null);
  return (
    <div className="space-y-3 text-[12.5px] text-muted-foreground">
      <div className="flex items-center gap-2">
        <span className="w-24 shrink-0">Repository</span>
        <code className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-foreground">
          {repository}
        </code>
        <CopyButton value={repository} label="Copy repository" />
      </div>
      <div className="flex items-center gap-2">
        <span className="w-24 shrink-0">Password</span>
        {password !== null ? (
          <>
            <code className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-foreground">
              {password}
            </code>
            <CopyButton value={password} label="Copy password" />
          </>
        ) : admin ? (
          <Button
            variant="secondary"
            size="xs"
            onClick={async () =>
              setPassword(await api.revealBackupPassword(teamId))
            }
          >
            <KeyIcon size={12} /> Reveal (logged)
          </Button>
        ) : (
          <span>Team owners and admins can reveal it.</span>
        )}
      </div>
      <pre className="overflow-x-auto rounded-lg border border-border bg-muted/40 p-3 font-mono text-[11.5px] leading-5 text-foreground/85">
        {`export RESTIC_REPOSITORY='${repository}'
${
          volumeId
            ? `restic snapshots --tag volume=${volumeId}
restic restore <snapshot> --target ./restore`
            : `restic snapshots --tag db=<database id>       # or --tag svc=<service id>
restic restore <snapshot> --target ./restore
# Databases: ./restore/dump holds the dump, e.g. pg_restore -d app postgres.dump`
        }`}
      </pre>
    </div>
  );
}
