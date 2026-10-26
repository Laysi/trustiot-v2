import { type ReactNode, useId } from "react";

/**
 * The layout of the full-page messages: not found, no permission, suspended, unavailable, errors,
 * and the screens a session without a tenant sees. One heading, what happened and what to do.
 */
export function StatusPage({
  title,
  icon,
  children,
  actions,
}: {
  title: string;
  icon?: ReactNode;
  children: ReactNode;
  actions?: ReactNode;
}) {
  const id = useId();
  return (
    <section
      className="mx-auto flex w-full max-w-lg flex-col items-center gap-4 py-12 text-center"
      aria-labelledby={id}
    >
      {icon ? (
        <div className="flex size-12 items-center justify-center rounded-full bg-muted text-muted-foreground [&_svg]:size-6">
          {icon}
        </div>
      ) : null}
      <h1 id={id} className="text-2xl font-semibold tracking-tight text-balance">
        {title}
      </h1>
      <div className="grid w-full gap-3 text-muted-foreground text-pretty">{children}</div>
      {actions ? <div className="mt-2 flex flex-wrap justify-center gap-2">{actions}</div> : null}
    </section>
  );
}
