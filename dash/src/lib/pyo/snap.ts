// Midtrans Snap on the page: snap.js opens the payment over the dashboard
// instead of sending the user to Midtrans' payment page. The script loads the
// first time someone pays. The client key it needs is public by design.

/** What Snap tells its callbacks. */
export type SnapResult = {
  status_code?: string;
  status_message?: string | string[];
  transaction_status?: string;
  order_id?: string;
};

type Snap = {
  pay: (
    token: string,
    callbacks: {
      onSuccess?: (r: SnapResult) => void;
      onPending?: (r: SnapResult) => void;
      onError?: (r: SnapResult) => void;
      onClose?: () => void;
    },
  ) => void;
};

declare global {
  interface Window {
    snap?: Snap;
  }
}

/** How a Snap payment ended. */
export type SnapOutcome =
  | { kind: "success" | "pending"; result: SnapResult }
  | { kind: "error"; message: string }
  | { kind: "closed" };

const ORIGIN = { sandbox: "https://app.sandbox.midtrans.com", production: "https://app.midtrans.com" };
const LOAD_MS = 15_000;

let loading: Promise<Snap> | null = null;

/** snap.js, loaded once. Rejects when it's blocked or doesn't load in time. */
export function loadSnap(clientKey: string, sandbox: boolean): Promise<Snap> {
  if (window.snap) return Promise.resolve(window.snap);
  loading ??= new Promise<Snap>((resolve, reject) => {
    const s = document.createElement("script");
    s.src = `${sandbox ? ORIGIN.sandbox : ORIGIN.production}/snap/snap.js`;
    s.async = true;
    s.id = "midtrans-script";
    s.dataset.clientKey = clientKey;
    const fail = () => {
      clearTimeout(timer);
      s.remove();
      loading = null;
      reject(new Error("Couldn't load Midtrans"));
    };
    const timer = setTimeout(fail, LOAD_MS);
    s.onerror = fail;
    s.onload = () => {
      if (!window.snap) return fail();
      clearTimeout(timer);
      resolve(window.snap);
    };
    document.head.appendChild(s);
  });
  return loading;
}

/** Opens Snap over the page. Throws at once if it can't, else settles when
 * the user is done with it. */
export function snapPay(snap: Snap, token: string): Promise<SnapOutcome> {
  let resolve!: (o: SnapOutcome) => void;
  const done = new Promise<SnapOutcome>((r) => (resolve = r));
  snap.pay(token, {
    onSuccess: (result) => resolve({ kind: "success", result }),
    onPending: (result) => resolve({ kind: "pending", result }),
    onError: (r) => {
      const m = Array.isArray(r.status_message) ? r.status_message.join(" ") : r.status_message;
      resolve({ kind: "error", message: m || "Midtrans couldn't take the payment" });
    },
    onClose: () => resolve({ kind: "closed" }),
  });
  return done;
}
