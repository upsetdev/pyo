import { createFileRoute } from "@tanstack/react-router";
import { newDatabaseRoute } from "~/components/pyo/database/NewDatabase";

export const Route = createFileRoute("/_app/$team/mysql/new")(newDatabaseRoute("mysql"));
