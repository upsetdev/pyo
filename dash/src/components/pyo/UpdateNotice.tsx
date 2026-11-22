import { ArrowSquareOutIcon, CloudArrowUpIcon, XIcon } from "@phosphor-icons/react";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { usePyo } from "~/lib/pyo/store";

// A quiet note at the foot of the sidebar when a newer Pyo is out: the
// version, its release notes and the Updates page. For the installation's
// admin, until they dismiss that version.

const DISMISSED_KEY = "pyo:update-dismissed";

function loadDismissed() {
  try {
    return localStorage.getItem(DISMISSED_KEY);
  } catch {
    return null;
  }
}

export function UpdateNotice({ managed }: { managed: boolean }) {
  const s = usePyo();
  const u = s.updates;
  const [dismissed, setDismissed] = useState(loadDismissed);
  const latest = u?.latest;
  if (
    managed ||
    !s.user.isAdmin ||
    !u ||
    !latest ||
    !u.plan?.pyo.needed ||
    u.active ||
    dismissed === latest.version
  )
    return null;
  const dismiss = () => {
    setDismissed(latest.version);
    try {
      localStorage.setItem(DISMISSED_KEY, latest.version);
    } catch {
      // Private windows: hidden until the next load.
    }
  };
  return (
    <aside
      aria-label="Update available"
      className="mx-3 mb-3 shrink-0 rounded-xl border border-frame-border bg-card px-3 py-2.5"
    >
      <div className="flex items-start gap-2">
        <CloudArrowUpIcon size={15} aria-hidden className="mt-px shrink-0 text-info" />
        <p className="min-w-0 flex-1 text-[12.5px] font-medium leading-5">
          Pyo {latest.version} is out
        </p>
        <button
          type="button"
          onClick={dismiss}
          aria-label={`Dismiss Pyo ${latest.version}`}
          title="Dismiss"
          className="-mr-1 -mt-0.5 inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-muted-foreground transition hover:bg-accent hover:text-foreground"
        >
          <XIcon size={12} />
        </button>
      </div>
      <p className="mt-1 flex items-center gap-3 pl-[23px] text-[12px]">
        <Link
          to="/updates"
          className="font-medium text-foreground/85 hover:text-foreground"
        >
          Update
        </Link>
        <a
          href={latest.url}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-1 text-muted-foreground hover:text-foreground"
        >
          Release notes
          <ArrowSquareOutIcon size={11} aria-hidden />
        </a>
      </p>
    </aside>
  );
}
