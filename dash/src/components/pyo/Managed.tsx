import { LifebuoyIcon } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import { Layered } from "~/components/ui/Layered";
import { EmptyState } from "~/components/ui/Panel";
import { fetchMeta } from "~/lib/pyo/backend";
import { useQuery } from "~/lib/pyo/query";

const SUPPORT = "support@pyo.dev";

/**
 * On a hosted Pyo (the API lists the "managed" feature), installation
 * settings the Pyo team runs show a note instead of their controls.
 * Self-hosted shows `children` as before. Nothing shows until the API has
 * said which it is, so the controls never flash on a hosted build.
 */
export function ManagedOr({
  title,
  description,
  children,
}: {
  title: string;
  description: ReactNode;
  children: ReactNode;
}) {
  const meta = useQuery("meta", fetchMeta);
  if (meta.status === "loading") return null;
  const managed =
    meta.status === "success" && meta.data.features.includes("managed");
  if (!managed) return <>{children}</>;
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
