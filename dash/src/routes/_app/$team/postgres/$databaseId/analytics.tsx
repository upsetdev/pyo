import { createFileRoute } from "@tanstack/react-router";
import { DatabaseAnalyticsPage, analyticsSearch } from "~/components/pyo/database/Analytics";

export const Route = createFileRoute("/_app/$team/postgres/$databaseId/analytics")({ validateSearch: analyticsSearch, component: DatabaseAnalyticsPage });
