import { createFileRoute } from "@tanstack/react-router";
import { DatabaseLogsPage, logsSearch } from "~/components/pyo/database/Logs";

export const Route = createFileRoute("/_app/$team/kv/$databaseId/logs")({ validateSearch: logsSearch, component: DatabaseLogsPage });
