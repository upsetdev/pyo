import { createFileRoute } from "@tanstack/react-router";
import { CertificatesList } from "~/components/pyo/Certificates";

// The team's uploaded TLS certificates, shared by its services' domains.
export const Route = createFileRoute("/_app/$team/certificates/")({
  component: CertificatesList,
});
