import { createFileRoute } from "@tanstack/react-router";
import { missingTabRoute } from "~/components/pyo/database/DatabaseLayout";

// MySQL has no extensions: the database's overview.
export const Route = createFileRoute("/_app/$team/mysql/$databaseId/extensions")(missingTabRoute);
