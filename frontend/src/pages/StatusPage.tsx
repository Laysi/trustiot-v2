import type { ReactNode } from "react";

/** The layout of the full-page messages: not found, no permission, unavailable, errors. */
export function StatusPage({ title, children, actions }: { title: string; children: ReactNode; actions?: ReactNode }) {
  return (
    <section className="status-page" aria-labelledby="status-page-title">
      <h1 id="status-page-title">{title}</h1>
      <div className="status-page__body">{children}</div>
      {actions ? <div className="status-page__actions">{actions}</div> : null}
    </section>
  );
}
