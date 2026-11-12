import { createFileRoute } from "@tanstack/react-router";
import { DatabaseLogsPage, logsSearch } from "~/components/pyo/database/Logs";

export const Route = createFileRoute("/_app/$team/mysql/$databaseId/logs")({ validateSearch: logsSearch, component: DatabaseLogsPage });
