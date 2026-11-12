import { createCsrfMiddleware, createStart } from "@tanstack/react-start";

// CSRF protection for server functions and form-style POSTs. Webhooks are
// skipped: Polar and Midtrans post cross-site by design and are verified by
// their signatures instead.
const csrf = createCsrfMiddleware({
  filter: ({ request, pathname }) =>
    request.method !== "GET" &&
    request.method !== "HEAD" &&
    !pathname.startsWith("/api/webhooks/"),
});

export const startInstance = createStart(() => ({
  requestMiddleware: [csrf],
}));
