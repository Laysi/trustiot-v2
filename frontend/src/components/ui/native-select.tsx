import { ChevronDownIcon } from "lucide-react";
import type { ComponentProps } from "react";

import { cn } from "../../lib/utils.ts";
import { controlClasses } from "./input.tsx";

/**
 * The browser's own select, styled like the other controls. Used instead of Radix's Select, whose
 * viewport renders an inline `<style>` element that the CSP refuses (frontend/README.md). The
 * native one also gives keyboard type-ahead over long lists (time zones) and the phone's picker.
 */
export function NativeSelect({ className, children, ...props }: ComponentProps<"select">) {
  return (
    <div data-slot="native-select" className="relative w-full">
      <select className={cn(controlClasses, "appearance-none pr-9", className)} {...props}>
        {children}
      </select>
      <ChevronDownIcon className="pointer-events-none absolute top-1/2 right-3 size-4 -translate-y-1/2 text-muted-foreground" />
    </div>
  );
}
