/** A small link to a docs page (pyo.dev/docs/<page>), instead of explaining in place. */
export function LearnMore({ page, className = "" }: { page: string; className?: string }) {
  return (
    <a
      href={`https://pyo.dev/docs/${page}`}
      target="_blank"
      rel="noreferrer"
      className={`whitespace-nowrap text-[12px] text-muted-foreground hover:text-foreground ${className}`}
    >
      Learn more
    </a>
  );
}
