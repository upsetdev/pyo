import { useRef } from "react";

/**
 * A form's checks against live data (a name that's taken, an email that's
 * already invited), as they stood before its own submit.
 *
 * What a form makes reaches the store while the form is still on screen: at
 * once for changes the store makes itself, with the next snapshot for the
 * rest. Checked live, the form would find its own name taken and turn red
 * until the page moves on. While `held` the value stays as it last was, so
 * hold from the submit until the form goes, and let go when the submit fails.
 */
export function useHeld<T>(value: T, held: boolean): T {
  const last = useRef(value);
  if (!held) last.current = value;
  return last.current;
}
