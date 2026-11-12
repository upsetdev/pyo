import { createFileRoute } from "@tanstack/react-router";
import { DatabaseBackupsPage } from "~/components/pyo/database/Backups";

export const Route = createFileRoute("/_app/$team/mysql/$databaseId/backups")({ component: DatabaseBackupsPage });
