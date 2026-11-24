// Request logs: what each of a team's clusters keeps of its requests, on
// its own servers. The API keeps a team's choice within the same bounds
// (pyo-proto's traffic.rs, MAX_DAYS_RANGE).

/** The days a team can keep its request logs. The last is the most. */
export const REQUEST_LOG_DAYS = [1, 3, 7, 14, 31];

/** The longest anything about a request stays, full IP addresses included. */
export const REQUEST_LOG_MAX_DAYS = REQUEST_LOG_DAYS[REQUEST_LOG_DAYS.length - 1];
