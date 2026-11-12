import { createFileRoute } from "@tanstack/react-router";
import { databaseRoute } from "~/components/pyo/database/DatabaseLayout";
import { seo } from "~/lib/seo";

export const Route = createFileRoute("/_app/$team/postgres/$databaseId")({
  head: () => ({ meta: seo({ title: "Database" }) }),
  ...databaseRoute("postgres"),
});
