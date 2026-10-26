import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * Joins class names and lets a later Tailwind class win over an earlier one of the same kind, so a
 * caller's `className` (`w-full`, `text-destructive`) overrides a component's default. The helper
 * every shadcn component expects (src/components/ui).
 */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
