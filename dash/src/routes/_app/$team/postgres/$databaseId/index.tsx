import { createFileRoute } from "@tanstack/react-router";
import { DatabaseOverview } from "~/components/pyo/database/Overview";

export const Route = createFileRoute("/_app/$team/postgres/$databaseId/")({ component: DatabaseOverview });
