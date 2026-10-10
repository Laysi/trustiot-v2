// ClickHouse over its HTTP interface, published on 127.0.0.1. Credentials go in headers, so they
// never appear in a URL, a log line or an argument list.

export interface ClickHouseTarget {
  port: number;
  user?: string;
  password?: string;
  database?: string;
}

export class ClickHouseError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

/** Runs one statement and returns the response body. */
export async function clickhouse(
  target: ClickHouseTarget,
  sql: string,
  timeoutMs = 20_000,
): Promise<string> {
  const headers: Record<string, string> = { "Content-Type": "text/plain; charset=utf-8" };
  if (target.user !== undefined) headers["X-ClickHouse-User"] = target.user;
  if (target.password !== undefined) headers["X-ClickHouse-Key"] = target.password;
  if (target.database !== undefined) headers["X-ClickHouse-Database"] = target.database;
  const response = await fetch(`http://127.0.0.1:${target.port}/`, {
    method: "POST",
    headers,
    body: sql,
    signal: AbortSignal.timeout(timeoutMs),
  });
  const text = await response.text();
  if (!response.ok) {
    // ClickHouse's error text names the statement problem, not the credentials.
    throw new ClickHouseError(`ClickHouse answered ${response.status}: ${text.trim().split("\n")[0]}`, response.status);
  }
  return text;
}

export async function ping(port: number, timeoutMs = 3000): Promise<boolean> {
  try {
    const response = await fetch(`http://127.0.0.1:${port}/ping`, {
      signal: AbortSignal.timeout(timeoutMs),
    });
    return response.ok;
  } catch {
    return false;
  }
}
