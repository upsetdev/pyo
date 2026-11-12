import { useState } from "react";

/**
 * A person's Gravatar (www.gravatar.com), or their initials when they have
 * none: d=404 makes Gravatar answer "not found" instead of a placeholder.
 */
export function UserAvatar({
  name,
  email,
  gravatar,
  size = 32,
  className = "",
}: {
  name: string;
  email?: string;
  gravatar?: string;
  size?: number;
  className?: string;
}) {
  const [failed, setFailed] = useState(false);
  const initials =
    (name || email || "?")
      .split(/[\s@.]+/)
      .filter(Boolean)
      .map((x) => x[0])
      .join("")
      .slice(0, 2)
      .toUpperCase() || "?";
  return (
    <span
      className={`relative inline-flex shrink-0 items-center justify-center overflow-hidden rounded-full bg-primary/15 font-semibold text-primary ring-1 ring-border ${className}`}
      style={{
        width: size,
        height: size,
        fontSize: Math.max(9, Math.round(size * 0.32)),
      }}
    >
      {initials}
      {gravatar && !failed ? (
        <img
          src={`https://www.gravatar.com/avatar/${gravatar}?s=${size * 2}&d=404`}
          alt=""
          width={size}
          height={size}
          loading="lazy"
          referrerPolicy="no-referrer"
          onError={() => setFailed(true)}
          className="absolute inset-0 h-full w-full object-cover"
        />
      ) : null}
    </span>
  );
}
