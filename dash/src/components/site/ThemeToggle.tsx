import { MoonIcon, SunIcon } from "@phosphor-icons/react";
import { useTheme } from "./ThemeProvider";
import { useEffect, useState } from "react";

export function ThemeToggle({ className = "" }: { className?: string }) {
  const { resolvedTheme, setTheme } = useTheme();
  // The resolved theme is unknown during SSR; render the dark variant until
  // mounted so server and client markup match.
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const isLight = mounted && resolvedTheme === "light";

  return (
    <button
      type="button"
      onClick={() => setTheme(isLight ? "dark" : "light")}
      aria-label="Toggle theme"
      title="Toggle theme"
      className={`inline-flex h-8 w-8 items-center justify-center rounded-full text-muted-foreground transition hover:bg-accent hover:text-foreground ${className}`}
    >
      {isLight ? <MoonIcon size={15} /> : <SunIcon size={15} />}
    </button>
  );
}
