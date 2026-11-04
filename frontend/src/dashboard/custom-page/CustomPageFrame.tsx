// The custom page of a dashboard version (docs/architecture/ai-assistant.md, 「自訂頁面」): the page
// the main agent wrote runs in the sandbox frame (frontend/sandbox), and this component is the outer
// end. It shows the frame only while the sandbox runtime's latest report on its port says the
// document matches the data the outer page sent (`DisplayGuard`; frontend/sandbox/README.md,
// 「顯示時比對的強制」); otherwise the frame is hidden (transparent, inert and out of the
// accessibility tree: `applyView`) or, once closed, removed, and a notice says why.
//
//   <CustomPageFrame page={version.page} runtime={version.runtime} data={data} title={version.title}
//     versionCreatedAt={version.created_at} onRefresh={refresh} />
//
// It draws the whole card (docs/design/chat-dashboards/states.html, state 8): the title, the
// 「自訂圖表」 label saying the page is drawn by code the AI wrote and not a platform component, when
// the version was written (it passed the check before; the API has no time of the check itself),
// a source button for the version's datasets, and the frame.
//
// `data` is the version's datasets, computations and constants with this viewing's results
// (`DashboardData`, the same as the components take); the frame gets the results once none is
// loading, and again whenever one changes. A click on a marked value in the page opens the same
// source panel as the components.
import { ChevronDown, CircleAlert, Code, Database, RotateCw, ShieldCheck } from "lucide-react";
import {
  createContext,
  type ReactNode,
  useContext,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { type DeliveredDataset, SandboxHost, type SandboxHostOptions } from "../../../sandbox/src/host.ts";
import { MAX_FRAME_HEIGHT, RUNTIMES } from "../../../sandbox/src/protocol.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../../components/ui/dropdown-menu.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { DashboardEnvProvider, useDashboardEnv } from "../context.tsx";
import { type DashboardData, type DatasetState, stateOf } from "../data.ts";
import type { SourceRef } from "../model.ts";
import { SourcePanel } from "../SourcePanel.tsx";
import { deliveryOf, publicSpecOf, sourceOf } from "./data.ts";
import { type CloseReason, DisplayGuard, type GuardView } from "./guard.ts";

/** Where the SPA's host serves the sandbox's version directories (`services/web`). */
export const SANDBOX_BASE = "/sandbox";

/** The frame's height before the page reports its own, and the least it is given. */
const DEFAULT_HEIGHT = 360;
const MIN_HEIGHT = 160;

/** What the component needs of the host adapter; tests pass a stand-in (`CustomPageHostContext`). */
export interface CustomPageHost {
  readonly frame: HTMLIFrameElement;
  init: SandboxHost["init"];
  data(datasets: readonly DeliveredDataset[]): number;
  destroy(): void;
}

export type CreateCustomPageHost = (options: SandboxHostOptions) => CustomPageHost;

/** The host adapter the component uses: the real `SandboxHost`, or a test's stand-in. */
export const CustomPageHostContext = createContext<CreateCustomPageHost>((options) => new SandboxHost(options));

export interface CustomPageFrameProps {
  /** The version's page (`DashboardVersion.page`). */
  page: string;
  /** The runtime version the page was checked with (`DashboardVersion.runtime`). */
  runtime: number;
  /** The version's datasets, computations and constants, and this viewing's results. */
  data: DashboardData;
  /** The card's heading and the frame's accessible name, such as the version's title. */
  title: string;
  /**
   * When the version was written (RFC 3339, `DashboardVersion.created_at`). A version with a custom
   * page is written only after its page passed the check (`docs/architecture/ai-assistant.md`,
   * 「儀表板」 point 4), so the label says it passed the check before this time.
   */
  versionCreatedAt: string;
  /** Runs the datasets again; offered when one failed. */
  onRefresh?(): void;
}

const CLOSED_MESSAGES: Readonly<Record<CloseReason, MessageId>> = {
  navigated: "dashboard.custom_page.closed.navigated",
  stalled: "dashboard.custom_page.closed.stalled",
  comparison: "dashboard.custom_page.closed.comparison",
  corrections: "dashboard.custom_page.closed.corrections",
  unnotified: "dashboard.custom_page.closed.unnotified",
  unstable: "dashboard.custom_page.closed.unstable",
};

const STATUS_MESSAGES: Readonly<Record<"loading" | "checking" | "stale", MessageId>> = {
  loading: "dashboard.custom_page.loading",
  checking: "dashboard.custom_page.checking",
  stale: "dashboard.custom_page.stale",
};

const HIDDEN_VIEW: GuardView = { visible: false, status: { kind: "loading" }, height: null };

function frameHeight(height: number | null): number {
  return Math.min(MAX_FRAME_HEIGHT, Math.max(MIN_HEIGHT, height ?? DEFAULT_HEIGHT));
}

/** Whether two deliveries carry the same dataset states (the same objects). */
function sameStates(a: readonly DatasetState[] | null, b: readonly DatasetState[]): boolean {
  return a !== null && a.length === b.length && a.every((state, index) => state === b[index]);
}

/** Why the frame is not shown, over the space it takes. */
function Notice(props: { view: GuardView; onReload(): void }) {
  const { t } = useDashboardEnv();
  const { status } = props.view;
  switch (status.kind) {
    case "refused":
      return (
        <Alert variant="destructive">
          <CircleAlert aria-hidden="true" />
          <AlertTitle>{t("dashboard.custom_page.data_invalid")}</AlertTitle>
        </Alert>
      );
    case "loading":
    case "checking":
    case "stale":
      return (
        <p role="status" className="text-sm text-muted-foreground">
          {t(STATUS_MESSAGES[status.kind])}
        </p>
      );
    case "findings":
      return (
        <Alert variant="destructive">
          <CircleAlert aria-hidden="true" />
          <AlertTitle>{t("dashboard.custom_page.findings", { count: status.count })}</AlertTitle>
        </Alert>
      );
    case "error":
      return (
        <Alert variant="destructive">
          <CircleAlert aria-hidden="true" />
          {/* A fixed message only: the page's own text could say anything, numbers too. */}
          <AlertTitle>{t("dashboard.custom_page.error")}</AlertTitle>
        </Alert>
      );
    case "closed":
      return (
        <Alert variant="destructive">
          <CircleAlert aria-hidden="true" />
          <AlertTitle>{t(CLOSED_MESSAGES[status.reason])}</AlertTitle>
          <AlertDescription>
            <Button size="sm" variant="outline" className="text-foreground" onClick={props.onReload}>
              <RotateCw aria-hidden="true" />
              {t("dashboard.custom_page.reload")}
            </Button>
          </AlertDescription>
        </Alert>
      );
  }
}

/** The datasets whose run failed, with the outer page's reason, and a way to run them again. */
function FailedDatasets(props: { data: DashboardData; onRefresh?: () => void }) {
  const { t } = useDashboardEnv();
  const failed = props.data.datasets.flatMap((dataset) => {
    const state = stateOf(props.data, dataset.name);
    return state.status === "failed" ? [{ title: dataset.title, message: state.message }] : [];
  });
  if (failed.length === 0) return null;
  return (
    <Alert variant="destructive">
      <CircleAlert aria-hidden="true" />
      <AlertTitle>{t("dashboard.custom_page.datasets_failed")}</AlertTitle>
      <AlertDescription>
        <ul className="grid gap-0.5">
          {failed.map((entry) => (
            <li key={entry.title}>
              {t("dashboard.custom_page.dataset_failed", { dataset: entry.title, message: entry.message })}
            </li>
          ))}
        </ul>
        {props.onRefresh === undefined ? null : (
          <Button size="sm" variant="outline" className="text-foreground" onClick={props.onRefresh}>
            <RotateCw aria-hidden="true" />
            {t("dashboard.custom_page.refresh")}
          </Button>
        )}
      </AlertDescription>
    </Alert>
  );
}

/**
 * The card's header (docs/design/chat-dashboards/states.html, state 8): the title with the 「自訂圖表」
 * label, what the label means and when the version was written, and the source of the version's
 * datasets: one opens the source panel at once, several are a menu. Everything here comes from the
 * outer page's own copy of the version, never from the frame.
 */
function CardHeader(props: {
  headingId: string;
  title: string;
  versionCreatedAt: string;
  data: DashboardData;
  onSource(source: SourceRef): void;
}) {
  const { t, format } = useDashboardEnv();
  const { data, onSource, title } = props;
  const sourceLabel = t("dashboard.custom_page.source_of", { title });
  const datasets = data.datasets;
  const only = datasets.length === 1 ? datasets[0] : undefined;
  const button = (
    <>
      <Database aria-hidden="true" />
      {t("dashboard.source.title")}
    </>
  );
  return (
    <header className="flex min-w-0 flex-wrap items-start justify-between gap-x-3 gap-y-1">
      {/* At least 16rem wide: on a narrow screen the source button goes below, not beside it. */}
      <div className="grid min-w-0 flex-[1_1_16rem] gap-1">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <h3 id={props.headingId} className="min-w-0 text-base leading-snug font-semibold break-words">
            {title}
          </h3>
          <span
            data-testid="custom-page-label"
            className="inline-flex shrink-0 items-center gap-1 rounded-sm border border-input bg-card px-1.75 py-px text-xs font-medium text-foreground [&_svg]:size-3.5"
          >
            <Code aria-hidden="true" />
            {t("dashboard.custom_page.label")}
          </span>
        </div>
        <p
          data-testid="custom-page-label-description"
          className="flex min-w-0 items-start gap-1 text-xs text-muted-foreground [&_svg]:mt-px [&_svg]:size-3.5 [&_svg]:shrink-0"
        >
          <ShieldCheck aria-hidden="true" />
          <span className="min-w-0 text-pretty">
            {t("dashboard.custom_page.label_description")}
            <span aria-hidden="true">・</span>
            <span className="tabular-nums">
              {t("dashboard.custom_page.checked", { time: format.instant(props.versionCreatedAt) })}
            </span>
          </span>
        </p>
      </div>
      {only !== undefined ? (
        <Button
          variant="ghost"
          size="sm"
          className="-my-1 -mr-2 shrink-0"
          aria-label={sourceLabel}
          onClick={() => onSource({ kind: "rows", dataset: only.name, rows: [] })}
        >
          {button}
        </Button>
      ) : datasets.length > 1 ? (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="sm" className="-my-1 -mr-2 shrink-0" aria-label={sourceLabel}>
              {button}
              <ChevronDown aria-hidden="true" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            {datasets.map((dataset) => (
              <DropdownMenuItem
                key={dataset.name}
                onSelect={() => onSource({ kind: "rows", dataset: dataset.name, rows: [] })}
              >
                {dataset.title}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
      ) : null}
    </header>
  );
}

/**
 * Shows the frame only when `view` says so; a closed frame is removed (`destroy` also closes the
 * port). Called from the guard's callback, synchronously with the report that changed the view.
 *
 * A hidden frame is transparent, takes no pointer input and is inert (out of the tab order and the
 * accessibility tree), but it is not `visibility: hidden`: Chromium stops rendering a cross-origin
 * frame hidden that way (no animation frames, animations do not advance), so a page hidden for a
 * running animation, or before its render settled (two animation frames), would never get
 * compared clean and stay hidden for good. The comparison never reads the frame element's own
 * opacity: it is in the outer document.
 */
function applyView(host: CustomPageHost, view: GuardView): void {
  if (view.status.kind === "closed") {
    host.destroy();
    return;
  }
  const frame = host.frame;
  frame.style.opacity = view.visible ? "1" : "0";
  frame.style.pointerEvents = view.visible ? "auto" : "none";
  frame.inert = !view.visible;
  frame.setAttribute("aria-hidden", view.visible ? "false" : "true");
  frame.tabIndex = view.visible ? 0 : -1;
  frame.style.height = `${frameHeight(view.height)}px`;
}

/** Whether `applyView` left `frame` shown (tests use it too). */
export function frameShown(frame: HTMLIFrameElement): boolean {
  return frame.isConnected && frame.style.opacity === "1" && !frame.inert;
}

interface Session {
  host: CustomPageHost;
  guard: DisplayGuard;
  deliver(): void;
}

/** The custom page in the language and time zone of `DashboardEnvProvider`, with `theme`. */
export function CustomPageFrameView(props: CustomPageFrameProps & { theme: "light" | "dark" }) {
  const { page, runtime, data, title, theme, onRefresh, versionCreatedAt } = props;
  const env = useDashboardEnv();
  const { locale, timeZone, t } = env;
  const createHost = useContext(CustomPageHostContext);
  const container = useRef<HTMLDivElement>(null);
  const session = useRef<Session | null>(null);
  const latestData = useRef(data);
  latestData.current = data;
  const [view, setView] = useState<GuardView>(HIDDEN_VIEW);
  const [source, setSource] = useState<SourceRef | null>(null);
  const [reloads, setReloads] = useState(0);
  const headingId = useId();
  const supported = RUNTIMES.some((known) => known === runtime);
  // A different spec is a different page: start over (the init message carries it).
  const specKey = useMemo(() => JSON.stringify(publicSpecOf(data)), [data]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: a new frame for a new page, spec, runtime, language, time zone, theme or reload; data and title follow through refs and effects
  useLayoutEffect(() => {
    const element = container.current;
    const win = element?.ownerDocument.defaultView;
    if (!supported || element === null || element === undefined || win === null || win === undefined) return;
    const doc = element.ownerDocument;
    setView(HIDDEN_VIEW);
    setSource(null);
    // The frame's visibility follows the guard at once, in the guard's own callback: a finding or a
    // missed heartbeat hides it before React renders anything.
    const guard = new DisplayGuard(
      {
        setTimeout: (callback, ms) => win.setTimeout(callback, ms),
        clearTimeout: (handle) => win.clearTimeout(handle as number),
      },
      (next) => {
        applyView(host, next);
        setView(next);
      },
    );
    let initSent = false;
    let delivered: readonly DatasetState[] | null = null;
    const deliver = () => {
      if (!initSent) return;
      const current = latestData.current;
      const datasets = deliveryOf(current);
      if (datasets === null) return;
      const states = current.datasets.map((dataset) => stateOf(current, dataset.name));
      if (sameStates(delivered, states)) return;
      delivered = states;
      try {
        guard.dataSent(host.data(datasets));
      } catch {
        // A result the sandbox protocol refuses (host.data parses it): nothing is sent, and the
        // frame, which still shows older data, stays hidden until data is sent again.
        guard.dataRefused();
      }
    };
    const host: CustomPageHost = createHost({
      container: element,
      runtime: runtime as (typeof RUNTIMES)[number],
      base: SANDBOX_BASE,
      title,
      onMessage: (message) => {
        if (message.type !== "ready" || initSent) return;
        host.init({ page, spec: publicSpecOf(latestData.current), locale, tz: timeZone, theme, mode: "view" });
        initSent = true;
        guard.started();
        deliver();
      },
      onReport: (message) => {
        guard.report(message);
        if (message.type === "source" && guard.view.visible) {
          const found = sourceOf(message.reference, latestData.current);
          if (found !== null) setSource(found);
        }
      },
      onNavigate: () => guard.navigated(),
    });
    // Hidden from the first paint on: nothing of the page shows before a clean comparison.
    const frame = host.frame;
    frame.style.display = "block";
    frame.style.width = "100%";
    frame.style.border = "0";
    applyView(host, guard.view);
    const onVisibility = () => (doc.visibilityState === "hidden" ? guard.pause() : guard.resume());
    doc.addEventListener("visibilitychange", onVisibility);
    onVisibility();
    // A browser may slow the timers of a frame out of view: the guard does not count a gap in its
    // reports then.
    const intersections =
      typeof win.IntersectionObserver === "function"
        ? new win.IntersectionObserver((entries) => {
            const entry = entries.at(-1);
            if (entry !== undefined) guard.setOnScreen(entry.isIntersecting);
          })
        : null;
    intersections?.observe(frame);
    session.current = { host, guard, deliver };
    return () => {
      intersections?.disconnect();
      doc.removeEventListener("visibilitychange", onVisibility);
      guard.dispose();
      host.destroy();
      session.current = null;
    };
  }, [page, runtime, specKey, locale, timeZone, theme, reloads, createHost, supported]);

  // New results (a refresh) go to the page as a new generation once none is loading.
  // biome-ignore lint/correctness/useExhaustiveDependencies: runs for each new data object; the session reads it through its ref
  useEffect(() => {
    session.current?.deliver();
  }, [data]);

  useEffect(() => {
    session.current?.host.frame.setAttribute("title", title);
  }, [title]);

  let notice: ReactNode = null;
  if (!supported) {
    notice = (
      <Alert variant="destructive">
        <CircleAlert aria-hidden="true" />
        <AlertTitle>{t("dashboard.custom_page.runtime_unsupported", { runtime })}</AlertTitle>
      </Alert>
    );
  } else if (!view.visible) {
    notice = <Notice view={view} onReload={() => setReloads((count) => count + 1)} />;
  }

  return (
    <section
      aria-labelledby={headingId}
      data-custom-page=""
      // The same attribute the platform components carry, so a page's items can be told apart by kind.
      data-component="custom_page"
      className="grid min-w-0 gap-3 rounded-xl border bg-card p-4 text-card-foreground shadow-sm"
    >
      <CardHeader
        headingId={headingId}
        title={title}
        versionCreatedAt={versionCreatedAt}
        data={data}
        onSource={setSource}
      />
      <FailedDatasets data={data} onRefresh={onRefresh} />
      <div className="relative min-w-0" style={{ minHeight: view.status.kind === "closed" ? undefined : MIN_HEIGHT }}>
        <div ref={container} data-testid="custom-page-container" />
        {notice === null ? null : (
          <div data-testid="custom-page-notice" className="absolute inset-x-0 top-0 grid gap-2 p-1">
            {notice}
          </div>
        )}
      </div>
      {source !== null ? <SourcePanel source={source} data={data} onClose={() => setSource(null)} /> : null}
    </section>
  );
}

/** `CustomPageFrameView` in the user's language, time zone and theme. */
export function CustomPageFrame(props: CustomPageFrameProps) {
  const { locale } = useI18n();
  const { timeZone, theme } = usePreferences();
  return (
    <DashboardEnvProvider locale={locale} timeZone={timeZone}>
      <CustomPageFrameView {...props} theme={theme} />
    </DashboardEnvProvider>
  );
}
