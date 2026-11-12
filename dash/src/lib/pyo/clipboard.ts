// Copying text, also where the browser has no clipboard API: the dashboard
// is served over plain HTTP on its tailnet address, which isn't a secure
// context, so navigator.clipboard is missing there. Then a hidden textarea
// and document.execCommand("copy") do it.

/** Copy `text`; rejects when neither way works. */
export async function copyText(text: string): Promise<void> {
  if (typeof navigator !== "undefined" && navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Denied: try the old way.
    }
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.top = "0";
  area.style.left = "0";
  area.style.opacity = "0";
  area.style.pointerEvents = "none";
  // Inside an open <dialog>, so the copy works from modals too.
  (document.querySelector("dialog[open]") ?? document.body).appendChild(area);
  const selection = document.getSelection();
  const kept = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
  area.select();
  area.setSelectionRange(0, text.length);
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } finally {
    area.remove();
    if (kept && selection) {
      selection.removeAllRanges();
      selection.addRange(kept);
    }
  }
  if (!ok) throw new Error("Copying isn't available here.");
}
