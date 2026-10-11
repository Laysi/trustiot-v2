// Health and security checks of a running environment. Every check reaches the component the
// way a developer or a service would (the published loopback port, or the container network)
// and also proves that a wrong or missing credential is refused.
import { connect } from "node:net";

import { clickhouse, ClickHouseError, ping } from "./clickhouse.ts";
import { connackCode } from "./mqtt.ts";
import { podman } from "./podman.ts";
import { askAuthentication, askAuthenticationTls, AUTH_SASL, isCertificateRefusal } from "./pgwire.ts";
import type { State } from "./state.ts";
import { namesFor, POSTGRES_CA_PATH, SERVICE_PORT } from "./stack.ts";
import { generateDevCertificates } from "./tls.ts";
import type { ServiceDef } from "./services.ts";

export interface CheckResult {
  name: string;
  ok: boolean;
  detail: string;
}

type Check = () => Promise<CheckResult>;

function result(name: string, ok: boolean, detail: string): CheckResult {
  return { name, ok, detail };
}

async function guarded(name: string, body: () => Promise<CheckResult>): Promise<CheckResult> {
  try {
    return await body();
  } catch (error) {
    return result(name, false, error instanceof Error ? error.message : String(error));
  }
}

export function tcpOpen(port: number, host = "127.0.0.1", timeoutMs = 2000): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = connect({ host, port });
    const done = (ok: boolean) => {
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(timeoutMs, () => done(false));
    socket.once("connect", () => done(true));
    socket.once("error", () => done(false));
  });
}

/** Passes when the attempt fails because the client refused the certificate. */
async function expectRefusal(name: string, attempt: () => Promise<unknown>): Promise<CheckResult> {
  try {
    const answer = await attempt();
    return result(name, false, `the connection was accepted (${String(answer)})`);
  } catch (error) {
    const code = (error as { code?: string }).code;
    // Another failure, such as an unreachable server, proves nothing about the certificate.
    return isCertificateRefusal(error)
      ? result(name, true, `refused (${code})`)
      : result(name, false, error instanceof Error ? error.message : String(error));
  }
}

/** Runs psql inside the Postgres container, logging in over TCP with the given password. */
async function psql(
  state: State,
  password: string,
  query: string,
): Promise<{ code: number; stdout: string; stderr: string }> {
  const names = namesFor(state.instance);
  const { user, database } = state.credentials.postgres;
  // The password goes to the container on stdin and becomes PGPASSWORD only inside the wrapper
  // shell, so it is on neither a command line nor a file.
  return await podman(
    [
      "exec",
      "--interactive",
      "--env",
      "PGSSLMODE=verify-full",
      "--env",
      `PGSSLROOTCERT=${POSTGRES_CA_PATH}`,
      names.container("postgres"),
      "sh",
      "-c",
      'IFS= read -r PGPASSWORD && export PGPASSWORD && exec psql "$@"',
      "psql-wrapper",
      // The container's own network address, not 127.0.0.1: every non-local address needs a
      // password (see POSTGRES_HBA), so this is a real password test. It connects the way a
      // service does: TLS, with the certificate verified against the dev CA and the name.
      "--host=postgres",
      "--port=5432",
      `--username=${user}`,
      `--dbname=${database}`,
      "--no-psqlrc",
      "--tuples-only",
      "--no-align",
      "--quiet",
      "--command",
      query,
    ],
    { stdin: `${password}\n`, timeoutMs: 20_000 },
  );
}

/** A name no certificate of this environment carries. */
const UNKNOWN_HOST_NAME = "not-postgres.invalid";

/**
 * `caCertificate` is the PEM of the instance's dev CA (`tls/ca.crt` in the state directory).
 * The TLS checks connect to the published port the way a verify-full client does.
 */
export function postgresChecks(state: State, caCertificate: string): Check[] {
  const { user, database } = state.credentials.postgres;
  const port = state.ports.postgres;
  return [
    () =>
      guarded("postgres: published port", async () => {
        const open = await tcpOpen(port);
        return result("postgres: published port", open, `127.0.0.1:${port}`);
      }),
    () =>
      guarded("postgres: published port refuses a connection without TLS", async () => {
        // A startup message in the clear: the server must refuse it, whatever the credentials.
        const name = "postgres: published port refuses a connection without TLS";
        const answer = await askAuthentication(port, user, database);
        return result(name, answer === "error", answer === "error" ? "refused" : `authentication request ${answer}`);
      }),
    () =>
      guarded("postgres: published port demands a password over TLS", async () => {
        // TLS verified against the dev CA and the loopback address, then a startup message
        // without credentials: the server must answer with a SCRAM challenge, never
        // "authentication OK".
        const name = "postgres: published port demands a password over TLS";
        const answer = await askAuthenticationTls(port, user, database, {
          ca: caCertificate,
          serverName: "127.0.0.1",
        });
        return result(name, answer === AUTH_SASL, `authentication request ${answer} (10 = SCRAM)`);
      }),
    () =>
      guarded("postgres: certificate of another CA refused", async () => {
        const name = "postgres: certificate of another CA refused";
        const other = generateDevCertificates("other").caCertificate;
        return await expectRefusal(name, () =>
          askAuthenticationTls(port, user, database, { ca: other, serverName: "127.0.0.1" }),
        );
      }),
    () =>
      guarded("postgres: certificate for another host name refused", async () => {
        const name = "postgres: certificate for another host name refused";
        return await expectRefusal(name, () =>
          askAuthenticationTls(port, user, database, { ca: caCertificate, serverName: UNKNOWN_HOST_NAME }),
        );
      }),
    () =>
      guarded("postgres: login over TLS and Apache AGE", async () => {
        const name = "postgres: login over TLS and Apache AGE";
        const answer = await psql(
          state,
          state.credentials.postgres.password,
          "SELECT 'age ' || extversion || ', tls ' || (SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()) " +
            "FROM pg_extension WHERE extname = 'age'",
        );
        const text = answer.stdout.trim();
        return result(name, answer.code === 0 && /^age \S+, tls true$/.test(text), text || answer.stderr.trim() || "no answer");
      }),
    () =>
      guarded("postgres: wrong password refused", async () => {
        const answer = await psql(state, "not-the-password", "SELECT 1");
        // The server must have answered and refused; a missing server is not a pass.
        const refused = answer.code !== 0 && /password authentication failed/i.test(answer.stderr);
        return result(
          "postgres: wrong password refused",
          refused,
          refused ? "refused" : answer.code === 0 ? "accepted a bad login" : "no refusal from the server",
        );
      }),
  ];
}

export function clickhouseChecks(state: State): Check[] {
  const { port } = { port: state.ports.clickhouse };
  const login = { port, user: state.credentials.clickhouse.user, password: state.credentials.clickhouse.password };
  return [
    () =>
      guarded("clickhouse: published port", async () =>
        result("clickhouse: published port", await ping(port), `http://127.0.0.1:${port}/ping`),
      ),
    () =>
      guarded("clickhouse: login", async () => {
        const answer = (await clickhouse(login, "SELECT 1")).trim();
        return result("clickhouse: login", answer === "1", `SELECT 1 -> ${answer}`);
      }),
    () =>
      guarded("clickhouse: anonymous access refused", async () => {
        try {
          await clickhouse({ port }, "SELECT 1");
          return result("clickhouse: anonymous access refused", false, "anonymous query succeeded");
        } catch (error) {
          if (error instanceof ClickHouseError) {
            return result("clickhouse: anonymous access refused", true, `refused (${error.status})`);
          }
          throw error;
        }
      }),
  ];
}

export function mosquittoChecks(state: State): Check[] {
  const endpoint = { host: "127.0.0.1", port: state.ports.mqtt };
  const login = { user: state.credentials.mqtt.user, password: state.credentials.mqtt.password };
  return [
    () =>
      guarded("mqtt: login", async () => {
        const code = await connackCode(endpoint, login);
        return result("mqtt: login", code === 0, `CONNACK ${code}`);
      }),
    () =>
      guarded("mqtt: anonymous access refused", async () => {
        const code = await connackCode(endpoint, {});
        return result("mqtt: anonymous access refused", code !== 0, `CONNACK ${code}`);
      }),
    () =>
      guarded("mqtt: wrong password refused", async () => {
        const code = await connackCode(endpoint, { user: login.user, password: "not-the-password" });
        return result("mqtt: wrong password refused", code !== 0, `CONNACK ${code}`);
      }),
  ];
}

export function serviceChecks(state: State, services: readonly ServiceDef[]): Check[] {
  const checks: Check[] = [];
  for (const service of services) {
    const port = state.ports.services[service.name];
    for (const path of ["/healthz", "/readyz"]) {
      const name = `${service.name}: GET ${path} (container port ${SERVICE_PORT})`;
      checks.push(() =>
        guarded(name, async () => {
          const response = await fetch(`http://127.0.0.1:${port}${path}`, {
            signal: AbortSignal.timeout(3000),
          });
          return result(name, response.status === 200, `127.0.0.1:${port} -> ${response.status}`);
        }),
      );
    }
  }
  return checks;
}

export async function runChecks(checks: Check[]): Promise<CheckResult[]> {
  return await Promise.all(checks.map((check) => check()));
}

/** Runs the checks until all pass or the time runs out; returns the last results. */
export async function waitForChecks(
  checks: () => Check[],
  timeoutMs: number,
  intervalMs = 1500,
): Promise<CheckResult[]> {
  const deadline = Date.now() + timeoutMs;
  let last: CheckResult[] = [];
  for (;;) {
    last = await runChecks(checks());
    if (last.every((item) => item.ok) || Date.now() >= deadline) return last;
    await Bun.sleep(intervalMs);
  }
}
