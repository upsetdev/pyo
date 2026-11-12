// The browser side of passkeys (WebAuthn): navigator.credentials with the
// options the API sends (crates/pyo-api/src/passkeys.rs), bytes as base64url
// both ways.

import { passkeys, type PasskeyChallenge } from "./backend";
import type { User } from "./types";

function fromB64url(s: string): ArrayBuffer {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function toB64url(buf: ArrayBuffer | null | undefined): string | undefined {
  if (!buf) return undefined;
  let bin = "";
  for (const b of new Uint8Array(buf)) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

type Desc = { id: string; type: string; transports?: string[] };
const descriptors = (list: unknown) =>
  ((list as Desc[] | undefined) ?? []).map((c) => ({
    ...c,
    id: fromB64url(c.id),
  })) as PublicKeyCredentialDescriptor[];

export function passkeysSupported() {
  return (
    typeof window !== "undefined" &&
    typeof window.PublicKeyCredential === "function" &&
    !!navigator.credentials
  );
}

/** Autofill (conditional UI): passkeys offered in the email field. */
export async function passkeyAutofillSupported() {
  if (!passkeysSupported()) return false;
  try {
    return (
      (await PublicKeyCredential.isConditionalMediationAvailable?.()) ?? false
    );
  } catch {
    return false;
  }
}

/** The user dismissed the prompt, or it was aborted: nothing to report. */
export function passkeyCancelled(e: unknown) {
  return (
    e instanceof DOMException &&
    (e.name === "NotAllowedError" || e.name === "AbortError")
  );
}

/**
 * Sign in with a passkey. `signal` set: as autofill in the email field
 * (conditional mediation), waiting until one is picked or it's aborted.
 */
export async function signInWithPasskey(signal?: AbortSignal): Promise<User> {
  const { challengeId, publicKey: o }: PasskeyChallenge =
    await passkeys.loginStart();
  const cred = (await navigator.credentials.get({
    publicKey: {
      ...(o as unknown as PublicKeyCredentialRequestOptions),
      challenge: fromB64url(o.challenge as string),
      allowCredentials: descriptors(o.allowCredentials),
    },
    ...(signal ? { mediation: "conditional" as const, signal } : {}),
  })) as PublicKeyCredential | null;
  if (!cred) throw new DOMException("No passkey", "NotAllowedError");
  const r = cred.response as AuthenticatorAssertionResponse;
  return passkeys.loginFinish(challengeId, {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: toB64url(r.authenticatorData),
      clientDataJSON: toB64url(r.clientDataJSON),
      signature: toB64url(r.signature),
      userHandle: toB64url(r.userHandle),
    },
    extensions: {},
  });
}

/** Add a passkey to the signed-in account; the API checks the password first. */
export async function addPasskey(password: string) {
  const { challengeId, publicKey: o } = await passkeys.registerStart(password);
  const user = o.user as { id: string; name: string; displayName: string };
  const cred = (await navigator.credentials
    .create({
      publicKey: {
        ...(o as unknown as PublicKeyCredentialCreationOptions),
        challenge: fromB64url(o.challenge as string),
        user: { ...user, id: fromB64url(user.id) },
        excludeCredentials: descriptors(o.excludeCredentials),
      },
    })
    .catch((e) => {
      // This authenticator already has one of the account's passkeys.
      if (e instanceof DOMException && e.name === "InvalidStateError")
        throw new Error("This passkey is already added.");
      throw e;
    })) as PublicKeyCredential | null;
  if (!cred) throw new DOMException("No passkey", "NotAllowedError");
  const r = cred.response as AuthenticatorAttestationResponse;
  return passkeys.registerFinish(challengeId, {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: toB64url(r.attestationObject),
      clientDataJSON: toB64url(r.clientDataJSON),
      transports: r.getTransports?.() ?? [],
    },
    extensions: {},
  });
}
