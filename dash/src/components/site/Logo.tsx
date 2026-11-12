import { site } from "~/config/site";

/** Pyo mark (a duck, in the text colour) and wordmark. */
export function Logo({
  className = "",
  showName = true,
}: {
  className?: string;
  showName?: boolean;
}) {
  return (
    <span className={`inline-flex items-center gap-2 ${className}`}>
      <svg
        viewBox="0 0 24 24"
        className="h-[22px] w-[22px] shrink-0"
        aria-hidden="true"
      >
        <path
          fillRule="evenodd"
          d="M4.15 6.02C5.14 2.95 7.63 1 10.8 1C14.65 1 17.36 3.85 17.64 7.7C17.68 9.96 18.72 10.73 20.08 11.59C21.57 12.5 22.57 13.36 22.52 14.58C22.48 16.3 20.31 17.48 17.82 17.43C14.87 17.43 12.61 15.85 11.93 13.22C11.89 13.04 11.75 12.91 11.75 13.13C11.48 15.53 13.92 18.65 17.36 18.65C18.72 18.7 19.94 18.34 20.94 17.66C19.85 20.6 16.23 22.95 11.93 22.95C6.73 22.95 2.52 19.47 2.52 14.94C2.52 12.68 3.92 11.32 3.87 9.74C2.65 9.42 1.43 8.83 1.43 7.79C1.43 6.79 2.88 6.34 4.15 6.02ZM7.31 6.98C7.31 7.35 7.01 7.65 6.64 7.65C6.26 7.65 5.96 7.35 5.96 6.98C5.96 6.6 6.26 6.3 6.64 6.3C7.01 6.3 7.31 6.6 7.31 6.98ZM10.48 6.98C10.48 7.35 10.18 7.65 9.8 7.65C9.43 7.65 9.13 7.35 9.13 6.98C9.13 6.6 9.43 6.3 9.8 6.3C10.18 6.3 10.48 6.6 10.48 6.98Z"
          className="fill-foreground"
        />
      </svg>
      {showName ? (
        <span className="text-[15px] font-bold tracking-tight text-foreground">
          {site.name}
        </span>
      ) : null}
    </span>
  );
}
