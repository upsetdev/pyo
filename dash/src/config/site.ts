// Everything brand-specific lives here; the rest of the app reads from it.

export const site = {
  name: "Pyo",
  /** One line used in <title>, OG cards and the footer. */
  tagline: "Self-hosted deployments on k3s",
  description:
    "Deploy websites, APIs, databases and workers to your own k3s clusters from Git, container images or templates.",
  url:
    (import.meta.env.VITE_APP_URL as string | undefined) ||
    "http://localhost:8000",
  supportEmail: "support@pyo.dev",
  /** Documentation, linked from the sign-in pages. */
  docsUrl:
    (import.meta.env.VITE_DOCS_URL as string | undefined) ||
    "https://pyo.dev/docs",
  /** Legal entity shown in the policies and the footer. */
  company: "Pyo",
  /** Year the product launched, for the footer copyright range. */
  since: 2026,
  social: {
    x: "https://x.com/",
    github: "https://github.com/",
  },
} as const;
