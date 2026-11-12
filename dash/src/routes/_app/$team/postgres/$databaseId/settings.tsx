import { createFileRoute } from "@tanstack/react-router";
import { DatabaseSettings } from "~/components/pyo/database/Settings";

export const Route = createFileRoute("/_app/$team/postgres/$databaseId/settings")({ component: DatabaseSettings });
