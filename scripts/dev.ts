// Local development environment: Postgres (with Apache AGE), ClickHouse, Mosquitto and the
// Rust services, run with podman. Usage: bun scripts/dev.ts <command>. See scripts/README.md.
import { runCleanups } from "./lib/cleanup.ts";
import { context, down, printEnv, purge, seed, status, up } from "./lib/commands.ts";

const USAGE = `Usage: bun scripts/dev.ts <command>

Commands:
  up        start the environment and wait until every health check passes
  seed      load the development data (idempotent)
  status    run the health and security checks of a running environment
  env       print connection settings, including the generated passwords, as KEY=value lines
  down      stop and remove the containers and the network; keep the data volumes
  purge     remove everything the environment created: containers, network, data volumes,
            built images, credentials (add --images to also remove the pulled images)

Environment: TRUSTIOT_DEV_INSTANCE (default "default"), TRUSTIOT_DEV_POSTGRES_PORT,
TRUSTIOT_DEV_CLICKHOUSE_PORT, TRUSTIOT_DEV_MQTT_PORT, TRUSTIOT_DEV_PORT_<SERVICE>.
`;

async function main(argv: string[]): Promise<number> {
  const [command, ...flags] = argv;
  const known = ["up", "seed", "status", "env", "down", "purge"];
  if (!command || !known.includes(command) || flags.some((flag) => !(command === "purge" && flag === "--images"))) {
    console.error(USAGE);
    return command === "--help" || command === "help" ? 0 : 2;
  }
  const ctx = context();
  switch (command) {
    case "up":
      await up(ctx);
      return 0;
    case "seed":
      await seed(ctx);
      return 0;
    case "status":
      return (await status(ctx)) ? 0 : 1;
    case "env":
      printEnv(ctx);
      return 0;
    case "down":
      await down(ctx);
      return 0;
    default:
      await purge(ctx, { images: flags.includes("--images") });
      return 0;
  }
}

// An interrupt removes the temporary secret files, build directories and the instance lock
// before leaving; containers that were already started stay for `down` to remove.
process.on("SIGINT", () => {
  runCleanups();
  process.exit(130);
});
process.on("SIGTERM", () => {
  runCleanups();
  process.exit(143);
});

try {
  process.exitCode = await main(Bun.argv.slice(2));
} catch (error) {
  console.error(`error: ${error instanceof Error ? error.message : String(error)}`);
  process.exitCode = 1;
}
