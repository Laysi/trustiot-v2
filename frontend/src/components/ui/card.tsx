import type { ComponentProps } from "react";

import { cn } from "../../lib/utils.ts";

export function Card({ className, ...props }: ComponentProps<"section">) {
  return (
    <section
      data-slot="card"
      className={cn("flex flex-col gap-6 rounded-xl border bg-card py-6 text-card-foreground shadow-sm", className)}
      {...props}
    />
  );
}

export function CardHeader({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="card-header"
      className={cn("grid auto-rows-min grid-cols-[1fr_auto] items-start gap-x-4 gap-y-1.5 px-6", className)}
      {...props}
    />
  );
}

/** A card's heading; `as` picks the level that fits the page's outline (h2 by default). */
export function CardTitle({
  className,
  as: Heading = "h2",
  ...props
}: ComponentProps<"h2"> & { as?: "h1" | "h2" | "h3" }) {
  return <Heading data-slot="card-title" className={cn("text-lg leading-snug font-semibold", className)} {...props} />;
}

export function CardDescription({ className, ...props }: ComponentProps<"p">) {
  return (
    <p data-slot="card-description" className={cn("col-start-1 text-sm text-muted-foreground", className)} {...props} />
  );
}

/** Something at the header's end, such as a save status. */
export function CardAction({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="card-action"
      className={cn("col-start-2 row-span-2 row-start-1 self-start justify-self-end", className)}
      {...props}
    />
  );
}

export function CardContent({ className, ...props }: ComponentProps<"div">) {
  return <div data-slot="card-content" className={cn("px-6", className)} {...props} />;
}

export function CardFooter({ className, ...props }: ComponentProps<"div">) {
  return <div data-slot="card-footer" className={cn("flex items-center gap-2 px-6", className)} {...props} />;
}
