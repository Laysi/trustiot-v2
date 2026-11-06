import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";

import { cn } from "../../../lib/utils.ts";

/**
 * A small label of a state (docs/design/platform-admin/design.md, 「元件」). It always carries
 * words, and the callers add an icon where the state is not clear without one, so no state is
 * told by its color alone. Tones use the roles of global.css only: a plain `--secondary` ground
 * with the status color as text, `--warning-soft` for the state worth a second look.
 */
const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-sm px-2 py-0.5 text-xs font-medium whitespace-nowrap [&_svg]:size-3.5 [&_svg]:shrink-0",
  {
    variants: {
      tone: {
        neutral: "bg-secondary text-secondary-foreground",
        success: "bg-secondary text-success",
        warning: "bg-warning-soft text-warning",
        danger: "bg-secondary text-destructive",
        brand: "bg-brand-soft text-primary",
        outline: "border text-muted-foreground",
      },
    },
    defaultVariants: { tone: "neutral" },
  },
);

export type BadgeTone = NonNullable<VariantProps<typeof badgeVariants>["tone"]>;

export function Badge({ className, tone, ...props }: ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return <span data-slot="badge" className={cn(badgeVariants({ tone }), className)} {...props} />;
}
