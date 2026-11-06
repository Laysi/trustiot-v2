/** Rows that stand in for the list while it loads; announced once, not row by row. */
export function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
          <span className="h-3 w-2/5 animate-pulse rounded bg-muted" />
          <span className="h-3 w-1/6 animate-pulse rounded bg-muted" />
          <span className="h-3 w-1/5 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}
