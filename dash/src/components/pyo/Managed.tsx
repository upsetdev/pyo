import { LifebuoyIcon } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { Layered } from "~/components/ui/Layered";
import { EmptyState } from "~/components/ui/Panel";
import { fetchMeta } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";
import { usePyo } from "~/lib/pyo/store";

const SUPPORT = "support@pyo.dev";

/**
 * Whether this Pyo is hosted (the API lists the "managed" feature).
 * Undefined until the API has said, so hosted-only changes never flash.
 */
export function useHosted(): boolean | undefined {
  const meta = useQuery("meta", fetchMeta);
  if (meta.status === "loading") return undefined;
  return meta.status === "success" && meta.data.features.includes("managed");
}

/** This Pyo's version, once the API has said. */
export function usePyoVersion(): string | undefined {
  const meta = useQuery("meta", fetchMeta);
  return meta.status === "success" ? meta.data.version : undefined;
}

/** Whether version `a` is older than `b` ("0.4.5" < "0.5.0"). Unknown: no. */
export function olderVersion(a: string | undefined, b: string | undefined) {
  const parse = (v?: string) =>
    /^v?(\d+)\.(\d+)\.(\d+)/.exec(v ?? "")?.slice(1).map(Number);
  const x = parse(a);
  const y = parse(b);
  if (!x || !y) return false;
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] < y[i];
  return false;
}

/**
 * On a hosted Pyo (the API lists the "managed" feature), installation
 * settings the Pyo team runs show a note instead of their controls.
 * Self-hosted shows `children` as before. Nothing shows until the API has
 * said which it is, so the controls never flash on a hosted build.
 * `forAdmin`: the installation's admin (the Pyo team) still gets the
 * controls on a hosted Pyo, for settings only they can set up there.
 */
export function ManagedOr({
  title,
  description,
  forAdmin = false,
  children,
}: {
  title: string;
  description: ReactNode;
  forAdmin?: boolean;
  children: ReactNode;
}) {
  const managed = useHosted();
  const admin = !!usePyo().user.isAdmin;
  if (managed === undefined) return null;
  if (!managed || (forAdmin && admin)) return <>{children}</>;
  return (
    <Layered>
      <EmptyState
        icon={LifebuoyIcon}
        title={title}
        description={
          <>
            {description} Questions? Email us at{" "}
            <a
              href={`mailto:${SUPPORT}`}
              className="font-medium text-foreground hover:text-primary"
            >
              {SUPPORT}
            </a>
            .
          </>
        }
      />
    </Layered>
  );
}
