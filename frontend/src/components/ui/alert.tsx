import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";

import { cn } from "../../lib/utils.ts";

const alertVariants = cva(
  "grid w-full grid-cols-[0_1fr] items-start gap-y-0.5 rounded-lg border px-4 py-3 text-sm has-[>svg]:grid-cols-[1rem_1fr] has-[>svg]:gap-x-3 [&>svg]:size-4 [&>svg]:translate-y-0.5 [&>svg]:text-current",
  {
    variants: {
      variant: {
        default: "bg-card text-card-foreground",
        destructive:
          "border-destructive/50 bg-card text-destructive [&>[data-slot=alert-description]]:text-destructive",
      },
    },
    defaultVariants: { variant: "default" },
  },
);

/**
 * A message in the page. Errors keep `role="alert"` (read out at once); a notice that does not
 * need attention passes `role="status"`.
 */
export function Alert({
  className,
  variant,
  role = "alert",
  ...props
}: ComponentProps<"div"> & VariantProps<typeof alertVariants>) {
  return <div data-slot="alert" role={role} className={cn(alertVariants({ variant }), className)} {...props} />;
}

export function AlertTitle({ className, ...props }: ComponentProps<"p">) {
  return <p data-slot="alert-title" className={cn("col-start-2 font-medium", className)} {...props} />;
}

export function AlertDescription({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-description"
      className={cn(
        "col-start-2 grid justify-items-start gap-1 text-sm text-muted-foreground [&_p]:leading-relaxed",
        className,
      )}
      {...props}
    />
  );
}
