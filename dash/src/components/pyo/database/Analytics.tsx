import { useNavigate, useSearch } from "@tanstack/react-router";
import { z } from "zod";
import { DatabaseAnalytics, RANGE_LABEL, type Range } from "~/components/pyo/DatabaseAnalytics";
import { Segmented } from "~/components/ui/Field";
import { useDatabase } from "./DatabaseLayout";

export const analyticsSearch = z.object({ range: z.enum(["1h", "24h", "7d", "30d"]).optional().catch(undefined) });

export function DatabaseAnalyticsPage() {
  const d = useDatabase();
  const range: Range = (useSearch({ strict: false }) as z.infer<typeof analyticsSearch>).range ?? "24h";
  const navigate = useNavigate();
  if (!d) return null;
  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <Segmented
          label="Time range"
          value={range}
          onChange={(r) => void navigate({ to: ".", search: { range: r === "24h" ? undefined : r }, replace: true })}
          options={(Object.keys(RANGE_LABEL) as Range[]).map((r) => ({ value: r, label: r, ariaLabel: RANGE_LABEL[r], title: RANGE_LABEL[r] }))}
        />
      </div>
      <DatabaseAnalytics key={d.id} d={d} range={range} />
    </div>
  );
}
