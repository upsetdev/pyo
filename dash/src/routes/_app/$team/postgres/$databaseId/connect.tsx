import { createFileRoute } from "@tanstack/react-router";
import { DatabaseConnect } from "~/components/pyo/database/Connect";

export const Route = createFileRoute("/_app/$team/postgres/$databaseId/connect")({ component: DatabaseConnect });
