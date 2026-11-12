import { createFileRoute } from "@tanstack/react-router";
import { databaseListRoute } from "~/components/pyo/database/DatabaseList";

export const Route = createFileRoute("/_app/$team/kv/")(databaseListRoute("valkey"));
