import { createFileRoute } from "@tanstack/react-router";
import { DatabaseExtensions } from "~/components/pyo/database/Extensions";

export const Route = createFileRoute("/_app/$team/kv/$databaseId/extensions")({ component: DatabaseExtensions });
