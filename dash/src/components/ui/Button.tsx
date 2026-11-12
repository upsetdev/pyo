import {
  forwardRef,
  type AnchorHTMLAttributes,
  type ButtonHTMLAttributes,
} from "react";
import { AppLink } from "./AppLink";

// Pill buttons in three weights. One primary action per
// section; secondary for alternatives; ghost for tertiary actions inside
// cards, tables and menus; danger only to confirm a destructive action.
type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "xs" | "sm" | "md" | "lg" | "icon";

const base =
  "inline-flex shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-full font-medium transition-[background-color,border-color,color,box-shadow,transform] duration-200 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-canvas active:translate-y-px disabled:pointer-events-none disabled:opacity-50 disabled:shadow-none";

const variants: Record<Variant, string> = {
  primary: "btn-emerge text-primary-foreground",
  // A visible outline (3:1) and a card surface, so it reads as a button.
  secondary:
    "border border-border-strong bg-card text-foreground shadow-card hover:border-input-hover hover:bg-accent active:bg-foreground/[0.09] active:shadow-none",
  ghost:
    "text-muted-foreground hover:bg-accent hover:text-foreground active:bg-foreground/[0.09]",
  danger:
    "bg-destructive-solid text-white shadow-card hover:brightness-110 active:brightness-90",
};

const sizes: Record<Size, string> = {
  xs: "h-7 px-3 text-[12px]",
  sm: "h-8 px-3.5 text-[12px]",
  md: "h-10 px-5 text-[13px]",
  lg: "h-11 px-6 text-[14px]",
  icon: "h-8 w-8 text-[13px]",
};

export function buttonClass(
  variant: Variant = "primary",
  size: Size = "md",
  extra = "",
) {
  return `${base} ${variants[variant]} ${sizes[size]} ${extra}`;
}

export const Button = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: Size }
>(function Button(
  {
    variant = "primary",
    size = "md",
    className = "",
    type = "button",
    ...props
  },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      className={buttonClass(variant, size, className)}
      {...props}
    />
  );
});

/** Same look as Button, for navigation. */
export function ButtonLink({
  href,
  variant = "primary",
  size = "md",
  className = "",
  ...props
}: Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href"> & {
  href: string;
  variant?: Variant;
  size?: Size;
  preload?: false | "intent";
}) {
  return (
    <AppLink
      href={href}
      className={buttonClass(variant, size, className)}
      {...props}
    />
  );
}
