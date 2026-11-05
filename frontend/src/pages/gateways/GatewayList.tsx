import { ChevronDownIcon, ChevronRightIcon, TriangleAlertIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef } from "react";
import { Link } from "react-router";

import type { Gateway, OfflineReason } from "../../api/gateways.ts";
import type { GatewayHealth, GatewayMetrics } from "../../api/pending/gateways.ts";
import { DEVICES_PATH } from "../../app/paths.ts";
import { Button, focusRing } from "../../components/ui/button.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { cn } from "../../lib/utils.ts";
import { type GatewayFormat, useGatewayFormat } from "./format.ts";
import { StateMark } from "./PresenceBadge.tsx";

export interface GatewayRow {
  gateway: Gateway;
  /** `undefined` while the health data is missing (not loaded, or the request failed). */
  health: GatewayHealth | undefined;
}

const OFFLINE_REASONS: Readonly<Record<OfflineReason, MessageId>> = {
  shutdown: "gateways.offline.shutdown",
  connection_lost: "gateways.offline.connection_lost",
  heartbeat_timeout: "gateways.offline.heartbeat_timeout",
};

/** The widths a usage bar takes, in steps of 5 %: Tailwind needs whole class names, and a `style` attribute is not allowed (CSP). */
const BAR_WIDTHS = [
  "w-0",
  "w-[5%]",
  "w-[10%]",
  "w-[15%]",
  "w-[20%]",
  "w-[25%]",
  "w-[30%]",
  "w-[35%]",
  "w-[40%]",
  "w-[45%]",
  "w-[50%]",
  "w-[55%]",
  "w-[60%]",
  "w-[65%]",
  "w-[70%]",
  "w-[75%]",
  "w-[80%]",
  "w-[85%]",
  "w-[90%]",
  "w-[95%]",
  "w-full",
] as const;

function barWidth(percent: number): string {
  const step = Math.min(20, Math.max(0, Math.round(percent / 5)));
  return BAR_WIDTHS[step] as string;
}

const dash = <span aria-hidden="true">—</span>;

function StatusCell({ gateway, format }: { gateway: Gateway; format: GatewayFormat }) {
  const { t } = format;
  switch (gateway.presence) {
    case "online":
      return (
        <span className="grid gap-0.5">
          <StateMark tone="online">{t("gateways.presence.online")}</StateMark>
          {gateway.presence_changed_at ? (
            <span className="text-xs text-muted-foreground">
              {t("gateways.since", { time: format.monthDayTime(gateway.presence_changed_at) })}
            </span>
          ) : null}
        </span>
      );
    case "offline":
      return (
        <span className="grid gap-0.5">
          <StateMark tone="offline">
            {gateway.offline_reason
              ? t("gateways.presence.offline_reason", { reason: t(OFFLINE_REASONS[gateway.offline_reason]) })
              : t("gateways.presence.offline")}
          </StateMark>
          {gateway.presence_changed_at ? (
            <span className="text-xs text-muted-foreground">
              {t("gateways.since", { time: format.monthDayTime(gateway.presence_changed_at) })}
            </span>
          ) : null}
        </span>
      );
    case "unknown":
      return (
        <span className="grid gap-0.5">
          <StateMark tone="unknown">{t("gateways.presence.unknown")}</StateMark>
          <span className="text-xs text-muted-foreground">
            {t("gateways.bound_at", { time: format.monthDayTime(gateway.bound_at) })}
          </span>
        </span>
      );
  }
}

function LastReport({ gateway, format, now }: { gateway: Gateway; format: GatewayFormat; now: number }) {
  if (!gateway.last_seen_at) return dash;
  return (
    <time dateTime={gateway.last_seen_at} title={format.exact(gateway.last_seen_at)}>
      {format.relative(gateway.last_seen_at, now)}
    </time>
  );
}

/** The configuration the gateway runs against the one it should have; a difference is being applied. */
function ConfigCell({ gateway, format }: { gateway: Gateway; format: GatewayFormat }) {
  const { t } = format;
  const applied = gateway.applied_config_version ?? null;
  const desired = gateway.desired_config_version ?? null;
  if (applied === null && desired === null) return dash;
  if (desired === null) return <>{t("gateways.config.current", { version: applied ?? 0 })}</>;
  if (applied === desired) {
    return (
      <span className="grid gap-0.5">
        {t("gateways.config.current", { version: desired })}
        <span className="text-xs text-muted-foreground">{t("gateways.config.latest")}</span>
      </span>
    );
  }
  return (
    <StateMark tone="attention" className="font-normal">
      {applied === null
        ? t("gateways.config.applying_first", { to: desired })
        : t("gateways.config.applying", { from: applied, to: desired })}
    </StateMark>
  );
}

/** The operating flags that need a look, from the latest heartbeat. */
function flagsOf(metrics: GatewayMetrics | null | undefined, format: GatewayFormat): string[] {
  if (!metrics) return [];
  const { t } = format;
  const flags: string[] = [];
  if (metrics.undervoltage) flags.push(t("gateways.flag.power_low"));
  if (!metrics.clock_synced) flags.push(t("gateways.flag.clock_unsynced"));
  if (metrics.offline_buffer_rows > 0) {
    flags.push(t("gateways.flag.offline_buffer", { count: format.number(metrics.offline_buffer_rows) }));
  }
  return flags;
}

function DevicesCell({ row, format }: { row: GatewayRow; format: GatewayFormat }) {
  const { t } = format;
  if (!row.health) return dash;
  const counts = row.health.devices;
  const flags = flagsOf(row.health.metrics, format);
  const parts = (
    [
      ["gateways.devices.normal", counts.normal],
      ["gateways.devices.partial", counts.partial],
      ["gateways.devices.offline", counts.offline],
      ["gateways.devices.unknown", counts.unknown],
    ] as const
  )
    .filter(([, count]) => count > 0)
    .map(([key, count]) => t(key, { count }))
    .join(t("gateways.devices.separator"));
  return (
    <span className="grid justify-items-start gap-1">
      {counts.total === 0 ? (
        <span className="text-muted-foreground">{t("gateways.devices.none")}</span>
      ) : (
        <Link
          to={`${DEVICES_PATH}?gateway=${encodeURIComponent(row.gateway.hardware_id)}`}
          className={cn("rounded-sm underline-offset-4 hover:underline", focusRing)}
        >
          {t("gateways.devices.summary", { total: counts.total, parts })}
        </Link>
      )}
      {flags.length > 0 ? (
        <span className="flex flex-wrap gap-1">
          {flags.map((flag) => (
            <span
              key={flag}
              className="inline-flex items-center gap-1 rounded-sm bg-warning-soft px-1.5 py-0.5 text-xs text-foreground"
            >
              <TriangleAlertIcon aria-hidden="true" className="size-3 text-warning" />
              {flag}
            </span>
          ))}
        </span>
      ) : null}
    </span>
  );
}

function Meter({ label, percent }: { label: string; percent: number }) {
  return (
    <span role="img" aria-label={label} className="mt-1 block h-1.5 w-full overflow-hidden rounded-full bg-border">
      <span className={cn("block h-full rounded-full bg-primary", barWidth(percent))} />
    </span>
  );
}

/** The operating metrics of the latest heartbeat (Laysi/trustiot-v2#117) as the design's expanded row. */
function Details({ row, format }: { row: GatewayRow; format: GatewayFormat }) {
  const { t } = format;
  const metrics = row.health?.metrics ?? null;
  if (!row.health) return <p className="text-sm text-muted-foreground">{t("gateways.details.unavailable")}</p>;
  if (!metrics) return <p className="text-sm text-muted-foreground">{t("gateways.details.none")}</p>;
  const percent = (key: MessageId, value: number) => (
    <Metric name={t(key)}>
      {t("gateways.metric.percent", { value: Math.round(value) })}
      <Meter label={t("gateways.metric.percent_of", { name: t(key), value: Math.round(value) })} percent={value} />
    </Metric>
  );
  return (
    <div className="grid gap-3">
      <p className="text-xs text-muted-foreground">
        {t("gateways.details.heading", { time: format.exact(metrics.reported_at) })}
      </p>
      <dl className="grid grid-cols-[repeat(auto-fill,minmax(9rem,1fr))] gap-x-5 gap-y-3 text-sm">
        {percent("gateways.metric.cpu", metrics.cpu_percent)}
        {percent("gateways.metric.memory", metrics.memory_percent)}
        {percent("gateways.metric.data_partition", metrics.data_partition_percent)}
        <Metric name={t("gateways.metric.soc_temperature")}>
          {metrics.soc_temperature_c === null
            ? "—"
            : t("gateways.metric.celsius", { value: Math.round(metrics.soc_temperature_c) })}
        </Metric>
        <Metric name={t("gateways.metric.uptime")}>{format.uptime(metrics.uptime_secs)}</Metric>
        <Metric name={t("gateways.metric.restarts")}>{format.number(metrics.restarts_7d)}</Metric>
        <Metric name={t("gateways.metric.offline_buffer")}>
          {metrics.offline_buffer_rows === 0 || metrics.offline_buffer_oldest_at === null
            ? t("gateways.metric.buffer_empty")
            : t("gateways.metric.buffer_rows", {
                count: format.number(metrics.offline_buffer_rows),
                time: format.clock(metrics.offline_buffer_oldest_at),
              })}
        </Metric>
        <Metric name={t("gateways.metric.clock")}>
          {metrics.clock_synced ? t("gateways.metric.clock_ok") : t("gateways.metric.clock_bad")}
        </Metric>
        <Metric name={t("gateways.metric.certificate")}>
          {metrics.certificate_expires_at ? format.date(metrics.certificate_expires_at) : "—"}
        </Metric>
      </dl>
    </div>
  );
}

function Metric({ name, children }: { name: string; children: ReactNode }) {
  return (
    <div className="grid content-start gap-1">
      <dt className="text-muted-foreground">{name}</dt>
      <dd className="tabular-nums">{children}</dd>
    </div>
  );
}

const TARGET_ROW =
  "data-[target=true]:bg-brand-soft data-[target=true]:shadow-[inset_3px_0_0_var(--brand)] focus-visible:outline-none";

/**
 * The tenant's gateways as a table on a wide screen and as a list on a phone, the two forms the
 * design draws; the one that does not fit is hidden, so a screen reader reads one of them. The
 * gateway's hardware ID opens its health details in place (there is no page for one gateway).
 */
export function GatewayList({
  rows,
  now,
  expanded,
  onToggle,
  highlighted,
  focusTarget,
}: {
  rows: readonly GatewayRow[];
  now: number;
  expanded: ReadonlySet<string>;
  onToggle(hardwareId: string): void;
  /** The row with the highlight (a link's target, or the gateway just bound). */
  highlighted: string | null;
  /** The row a link named: it scrolls into view and takes the focus. */
  focusTarget: string | null;
}) {
  const format = useGatewayFormat();
  const { t } = format;
  return (
    <section aria-label={t("gateways.list_label")} className="rounded-lg border bg-card text-card-foreground">
      <table className="hidden w-full text-sm md:table">
        <caption className="sr-only">{t("gateways.caption", { count: rows.length })}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            {(
              [
                "gateways.column.gateway",
                "gateways.column.status",
                "gateways.column.last_report",
                "gateways.column.config",
                "gateways.column.devices",
                "gateways.column.agent",
              ] as const
            ).map((key) => (
              <th key={key} scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap first:px-4">
                {t(key)}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const id = row.gateway.hardware_id;
            const open = expanded.has(id);
            return (
              <TableRows
                key={id}
                id={id}
                highlighted={highlighted === id}
                focus={focusTarget === id}
                details={
                  open ? (
                    <DetailsRow id={`gw-${id}`}>
                      <Details row={row} format={format} />
                    </DetailsRow>
                  ) : null
                }
              >
                <th scope="row" className="px-4 py-3 text-left font-normal">
                  <Button
                    variant="ghost"
                    size="sm"
                    className="-ml-1 h-auto gap-1.5 px-1 py-0.5 font-mono"
                    aria-expanded={open}
                    {...(open ? { "aria-controls": `gw-${id}` } : {})}
                    onClick={() => onToggle(id)}
                  >
                    {open ? <ChevronDownIcon aria-hidden="true" /> : <ChevronRightIcon aria-hidden="true" />}
                    {id}
                  </Button>
                </th>
                <td className="px-3 py-3">
                  <StatusCell gateway={row.gateway} format={format} />
                </td>
                <td className="px-3 py-3 whitespace-nowrap">
                  <LastReport gateway={row.gateway} format={format} now={now} />
                </td>
                <td className="px-3 py-3">
                  <ConfigCell gateway={row.gateway} format={format} />
                </td>
                <td className="max-w-64 px-3 py-3">
                  <DevicesCell row={row} format={format} />
                </td>
                <td className="px-3 py-3 whitespace-nowrap">{row.gateway.agent_version ?? dash}</td>
              </TableRows>
            );
          })}
        </tbody>
      </table>
      <ul aria-label={t("gateways.caption", { count: rows.length })} className="grid divide-y md:hidden">
        {rows.map((row) => {
          const id = row.gateway.hardware_id;
          const open = expanded.has(id);
          return (
            <MobileItem key={id} id={id} highlighted={highlighted === id} focus={focusTarget === id}>
              <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
                <span className="font-mono font-medium">{id}</span>
                <StatusCell gateway={row.gateway} format={format} />
              </div>
              <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
                <span>
                  {t("gateways.mobile.last_report")} <LastReport gateway={row.gateway} format={format} now={now} />
                </span>
                <span>
                  {t("gateways.mobile.config")} <ConfigCell gateway={row.gateway} format={format} />
                </span>
                <span>
                  {t("gateways.mobile.agent")} {row.gateway.agent_version ?? dash}
                </span>
              </div>
              <DevicesCell row={row} format={format} />
              <Button
                variant="ghost"
                size="sm"
                className="-ml-2 justify-self-start"
                aria-expanded={open}
                aria-label={t("gateways.details.toggle", { gateway: id })}
                {...(open ? { "aria-controls": `gw-m-${id}` } : {})}
                onClick={() => onToggle(id)}
              >
                {open ? <ChevronDownIcon aria-hidden="true" /> : <ChevronRightIcon aria-hidden="true" />}
                {t("gateways.details.mobile")}
              </Button>
              {open ? (
                <div id={`gw-m-${id}`} className="rounded-lg bg-muted p-3">
                  <Details row={row} format={format} />
                </div>
              ) : null}
            </MobileItem>
          );
        })}
      </ul>
    </section>
  );
}

/** One gateway's table row (its cells are the children) and under it the row of its details, when they are open. */
function TableRows({
  id,
  highlighted,
  focus,
  details,
  children,
}: {
  id: string;
  highlighted: boolean;
  focus: boolean;
  details: ReactNode;
  children: ReactNode;
}) {
  const ref = useRef<HTMLTableRowElement>(null);
  useFocusTarget(ref, focus);
  return (
    <>
      <tr
        ref={ref}
        id={`gateway-${id}`}
        tabIndex={-1}
        data-target={highlighted ? "true" : undefined}
        className={cn("border-t", TARGET_ROW)}
      >
        {children}
      </tr>
      {details}
    </>
  );
}

function DetailsRow({ id, children }: { id: string; children: ReactNode }) {
  return (
    <tr id={id} className="border-t-0">
      <td colSpan={6} className="bg-muted px-6 py-4">
        {children}
      </td>
    </tr>
  );
}

function MobileItem({
  id,
  highlighted,
  focus,
  children,
}: {
  id: string;
  highlighted: boolean;
  focus: boolean;
  children: ReactNode;
}) {
  const ref = useRef<HTMLLIElement>(null);
  useFocusTarget(ref, focus);
  return (
    <li
      ref={ref}
      id={`gateway-m-${id}`}
      tabIndex={-1}
      data-target={highlighted ? "true" : undefined}
      className={cn("grid gap-2 px-4 py-3 text-sm", TARGET_ROW)}
    >
      {children}
    </li>
  );
}

/** Scrolls the element a link named into view and focuses it, once the link's target is set. */
function useFocusTarget(ref: { current: HTMLElement | null }, focus: boolean) {
  useEffect(() => {
    if (!focus) return;
    ref.current?.scrollIntoView?.({ block: "center" });
    ref.current?.focus({ preventScroll: true });
  }, [focus, ref]);
}
