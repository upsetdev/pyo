// What a team may choose for its observability: anything up to the
// installation's limit (teamobs.rs in the API keeps the same order).

import type { IpMode, QueryStrings } from "./types";

/** How much each keeps, least first. */
const RANK: Record<IpMode | QueryStrings, number> = {
  none: 0,
  anonymized: 1,
  names: 1,
  full: 2,
  keep: 2,
};

/** Whether a team may pick `value` under the installation's `limit`. */
export function withinLimit<T extends IpMode | QueryStrings>(value: T, limit: T) {
  return RANK[value] <= RANK[limit];
}
