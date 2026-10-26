import { type ReactNode, useId } from "react";

import { cn } from "../../lib/utils.ts";
import { Label } from "./label.tsx";

/** What a field passes to its control, so the label, hint and error are announced with it. */
export interface FieldControlProps {
  id: string;
  "aria-describedby"?: string;
  "aria-invalid"?: true;
}

/**
 * A labelled control with an optional hint and error. The error replaces nothing: the hint stays,
 * and the control is marked `aria-invalid` and described by both, so a screen reader reads the
 * error when the control gets focus.
 */
export function FormField({
  label,
  description,
  error,
  className,
  children,
}: {
  label: ReactNode;
  description?: ReactNode;
  error?: string | undefined;
  className?: string;
  children: (control: FieldControlProps) => ReactNode;
}) {
  const id = useId();
  const descriptionId = `${id}-description`;
  const errorId = `${id}-error`;
  const describedBy = [description ? descriptionId : null, error ? errorId : null].filter(Boolean).join(" ");
  // content-start keeps the label and control together when a neighbour in the same grid row is
  // taller (it has a hint or an error), so the controls of one row line up.
  return (
    <div data-slot="form-field" className={cn("grid content-start gap-2", className)}>
      <Label htmlFor={id}>{label}</Label>
      {children({
        id,
        ...(describedBy ? { "aria-describedby": describedBy } : {}),
        ...(error ? { "aria-invalid": true as const } : {}),
      })}
      {description ? (
        <p id={descriptionId} className="text-sm text-muted-foreground">
          {description}
        </p>
      ) : null}
      {error ? (
        <p id={errorId} className="text-sm font-medium text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}
