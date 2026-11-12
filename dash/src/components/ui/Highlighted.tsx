import { highlight, TOKEN_CLASS, type Lang } from "~/lib/pyo/highlight";

/** Text with simple token colouring (see lib/pyo/highlight). */
export function Highlighted({ text, lang }: { text: string; lang: Lang }) {
  return (
    <>
      {highlight(text, lang).map((t, i) =>
        TOKEN_CLASS[t.kind] ? (
          <span key={i} className={TOKEN_CLASS[t.kind]}>
            {t.text}
          </span>
        ) : (
          t.text
        ),
      )}
    </>
  );
}
