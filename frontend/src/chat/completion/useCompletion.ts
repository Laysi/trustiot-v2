import { useQuery } from "@tanstack/react-query";
import {
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import type { PopoverModel } from "./CompletionPopover.tsx";
import { useCompletionConfig } from "./config.ts";
import { type Group, mentionGroups, type Row, templateGroups } from "./groups.ts";
import { loadRecents, rememberRecent } from "./recents.ts";
import { type Entry, rangeEntries, useMentionSources } from "./sources.ts";
import { type QuestionTemplate, useTemplateSource } from "./templates.tsx";
import {
  applyEdit,
  detectTrigger,
  findSlots,
  intactMentions,
  type Mention,
  type MentionKind,
  type MentionTarget,
  mentionInsertion,
  neighbourSlot,
  reconcileMentions,
  type Slot,
} from "./text.ts";

/** A mention as it goes with the question: where it is in the text, and what it stands for. */
export interface SubmittedMention {
  start: number;
  end: number;
  label: string;
  target: MentionTarget;
}

type Session =
  | { mode: "mention"; start: number; query: string; now: number }
  | { mode: "template"; start: number; end: number; query: string }
  | { mode: "slot"; start: number; end: number; label: string; kind: MentionKind | null; now: number };

/** What the composer needs to hook the completion up (see `CompletionField` and `CompletionTools`). */
export interface Completion {
  boxRef: RefObject<HTMLTextAreaElement | null>;
  mirrorRef: RefObject<HTMLDivElement | null>;
  /** Handlers and ARIA attributes for the textarea, but its key handler: `onKeyDown` says whether it took the key. */
  textareaProps: {
    onChange: (event: ChangeEvent<HTMLTextAreaElement>) => void;
    onClick: () => void;
    onKeyUp: (event: KeyboardEvent<HTMLTextAreaElement>) => void;
    onBlur: () => void;
    onScroll: () => void;
    "aria-autocomplete": "list";
    "aria-controls": string | undefined;
    "aria-activedescendant": string | undefined;
  };
  /** Handles the keys of an open list and the blanks; true when it did, and the caller must not. */
  onKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => boolean;
  /** What the mirror layer draws on top of the text. */
  mirror: {
    mentions: Mention[];
    /** The blank that is selected, or `null`. */
    slotSelection: [number, number] | null;
    /** The `@` and the word typed after it, which is being looked up, or `null`. */
    query: [number, number] | null;
  };
  popover: PopoverModel | null;
  /** The mentions in the text now, for the question that is sent with it. */
  mentions: SubmittedMention[];
  /** Blanks of a template still to fill: the question is not sent while there are any. */
  slotsLeft: number;
  /** Says so when blanks are left, for the composer's status line; `null` otherwise. */
  notice: ReactNode;
  /** Whether the build offers question templates. */
  templatesOffered: boolean;
  openMentions: () => void;
  openTemplates: () => void;
  /** The text the live region reads out: the result count, or what was just inserted. */
  liveText: string | null;
  helpId: string;
}

/** Whether `active` has held for `ms`; false the moment it stops. */
function useAfterDelay(active: boolean, ms: number): boolean {
  const [passed, setPassed] = useState(false);
  useEffect(() => {
    if (!active) {
      setPassed(false);
      return;
    }
    const timer = setTimeout(() => setPassed(true), ms);
    return () => clearTimeout(timer);
  }, [active, ms]);
  return active && passed;
}

/** `value` after it has stayed the same for `ms`; `null` clears at once. */
function useSettled(value: string | null, ms: number): string | null {
  const [settled, setSettled] = useState<string | null>(null);
  useEffect(() => {
    if (value === null) {
      setSettled(null);
      return;
    }
    const timer = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return value === null ? null : settled;
}

const SLOT_BRACKETS = (label: string) => `［${label}］`;
const TEMPLATE_STALE_MS = 60_000;

/**
 * The deterministic completion of the composer (Laysi/trustiot-v2#155, design 「@ 提及」 and 「問句範本」):
 * an `@` opens the tenant's devices, data points, models and time ranges, a `/` at the start (or the
 * template button) opens the question templates, and a template's blanks are filled one by one.
 * Nothing here asks an LLM, and the lists are the tenant's own (`useMentionSources`).
 *
 * The composer keeps the text; this hook reads it through `value` and writes it through
 * `onChange`, and keeps what the text cannot hold: the open list, and what each mention stands
 * for (`mentions`, kept in step with every edit, so an edit inside one turns it back into text).
 */
export function useCompletion({ value, onChange }: { value: string; onChange: (value: string) => void }): Completion {
  const i18n = useI18n();
  const { t, locale } = i18n;
  const { timeZone } = usePreferences();
  const config = useCompletionConfig();
  const auth = useSession().session;
  const user = auth?.user.id ?? "";
  const tenantId = auth?.activeTenant?.id ?? "";
  const tenantName = auth?.activeTenant?.name ?? null;
  const templateSource = useTemplateSource();

  const boxRef = useRef<HTMLTextAreaElement | null>(null);
  const mirrorRef = useRef<HTMLDivElement | null>(null);
  const listId = useId();
  const helpId = useId();
  const [session, setSession] = useState<Session | null>(null);
  const [filter, setFilter] = useState<MentionKind | null>(null);
  const [activeKey, setActiveKey] = useState<string | null>(null);
  const [mentions, setMentions] = useState<Mention[]>([]);
  const [recents, setRecents] = useState<string[]>(() => loadRecents(user, tenantId));
  const [inserted, setInserted] = useState<string | null>(null);
  const [placement, setPlacement] = useState<"below" | "above">("below");
  const [selectionTick, setSelectionTick] = useState(0);
  const dismissed = useRef<{ mode: Session["mode"]; start: number } | null>(null);
  const pendingSelection = useRef<[number, number] | null>(null);
  const expectedValue = useRef(value);
  /** What each blank of the templates inserted so far is filled with, by the words in its brackets. */
  const slotKinds = useRef(new Map<string, MentionKind>());

  const live = useMemo(() => intactMentions(mentions, value), [mentions, value]);
  const slots = useMemo(() => findSlots(value), [value]);

  // Text the composer changed on its own (a sent question cleared, an address's question put in)
  // ends the open list: its position means nothing in the new text.
  useEffect(() => {
    if (value !== expectedValue.current) setSession(null);
    expectedValue.current = value;
  }, [value]);

  // The selection a keystroke or a pick asked for, once the text it refers to is in the box.
  // biome-ignore lint/correctness/useExhaustiveDependencies: runs after every render a new selection was asked for
  useLayoutEffect(() => {
    const selection = pendingSelection.current;
    const box = boxRef.current;
    if (selection === null || box === null) return;
    pendingSelection.current = null;
    box.setSelectionRange(selection[0], selection[1]);
  }, [selectionTick, value]);

  const open = session !== null;
  // Below the box unless there is no room under it (a box fixed at the bottom of the page or a phone's screen).
  useLayoutEffect(() => {
    if (!open) return;
    const form = boxRef.current?.closest("form");
    if (!form) return;
    setPlacement(window.innerHeight - form.getBoundingClientRect().bottom < 380 ? "above" : "below");
  }, [open]);

  const kind =
    session === null ? null : session.mode === "slot" ? session.kind : session.mode === "mention" ? filter : null;
  const query = session?.mode === "mention" ? session.query : "";
  const wantsLists = session !== null && session.mode !== "template";
  const wants = {
    devices: wantsLists && (kind === null || kind === "device"),
    models: wantsLists && (kind === null || kind === "model"),
    points:
      wantsLists &&
      (kind === null || kind === "point") &&
      (query !== "" || kind === "point" || recents.some((key) => key.startsWith("point:"))),
  };
  const sources = useMentionSources(tenantId, wants, t);
  const now = session !== null && session.mode !== "template" ? session.now : 0;
  const ranges = useMemo(
    () => (wantsLists ? rangeEntries(now, timeZone, locale, t) : []),
    [wantsLists, now, timeZone, locale, t],
  );
  const templatesQuery = useQuery({
    queryKey: ["question-templates", tenantId, locale],
    queryFn: ({ signal }) => (templateSource ? templateSource.listTemplates(locale, signal) : Promise.resolve([])),
    enabled: session?.mode === "template" && templateSource !== null,
    staleTime: TEMPLATE_STALE_MS,
  });

  const groups: Group[] = useMemo(() => {
    if (session === null) return [];
    if (session.mode === "template") {
      const status = templatesQuery.isError ? "error" : templatesQuery.isSuccess ? "ready" : "loading";
      return templateGroups(templatesQuery.data ?? [], session.query, status, t("chat.mention.loading"), (template) =>
        template.example
          ? t("chat.template.meta_example", { tool: template.tool, example: template.example })
          : template.tool,
      );
    }
    return mentionGroups({
      query,
      kind,
      sources,
      ranges,
      recents,
      titles: {
        recent: t("chat.mention.group.recent"),
        device: t("chat.mention.group.device"),
        point: t("chat.mention.group.point"),
        model: t("chat.mention.group.model"),
        range: t("chat.mention.group.range"),
      },
    });
  }, [
    session,
    query,
    kind,
    sources,
    ranges,
    recents,
    templatesQuery.data,
    templatesQuery.isError,
    templatesQuery.isSuccess,
    t,
  ]);

  const rows: Row[] = useMemo(() => groups.flatMap((group) => group.rows), [groups]);
  const activeIndex = Math.max(
    0,
    rows.findIndex((row) => row.key === activeKey),
  );
  const optionId = useCallback((index: number) => `${listId}-option-${index}`, [listId]);

  const failed =
    session?.mode === "template"
      ? templatesQuery.isError
      : session !== null && Object.values(sources.status).includes("error");
  const loading = groups.some((group) => group.loading);
  const skeleton = useAfterDelay(loading, config.skeletonDelayMs);
  // The count is read out once the lookups are over; while one runs there is nothing to count yet.
  const countText =
    !open || loading
      ? null
      : rows.length === 0
        ? t("chat.mention.count_none")
        : t("chat.mention.count_selected", { count: rows.length, name: rows[activeIndex]?.name ?? "" });
  const settledCount = useSettled(open ? countText : null, config.announceDelayMs);

  /** Writes the next text, the mentions that go with it, the selection and the list. */
  const commit = (next: {
    text: string;
    mentions: Mention[];
    selection: [number, number];
    session: Session | null;
  }) => {
    expectedValue.current = next.text;
    pendingSelection.current = next.selection;
    setMentions(next.mentions);
    setSession(next.session);
    setActiveKey(null);
    setFilter(null);
    dismissed.current = null;
    setSelectionTick((tick) => tick + 1);
    onChange(next.text);
  };

  const selectionOf = () => {
    const box = boxRef.current;
    return { start: box?.selectionStart ?? value.length, end: box?.selectionEnd ?? value.length };
  };

  const slotSession = (slot: Slot): Session => ({
    mode: "slot",
    start: slot.start,
    end: slot.end,
    label: slot.label,
    kind: slotKinds.current.get(slot.label) ?? null,
    now: config.now(),
  });

  const goToSlot = (slot: Slot, text: string, nextMentions: Mention[]) => {
    commit({ text, mentions: nextMentions, selection: [slot.start, slot.end], session: slotSession(slot) });
  };

  /** Looks at the text before the caret and opens, updates or closes the list accordingly. */
  const detect = (text: string, caret: number) => {
    const trigger = detectTrigger(text, caret, templateSource !== null);
    if (trigger === null) {
      dismissed.current = null;
      setSession(null);
      return;
    }
    const gone = dismissed.current;
    if (gone && gone.mode === trigger.mode && gone.start === trigger.start) {
      setSession(null);
      return;
    }
    dismissed.current = null;
    const same = session !== null && session.mode === trigger.mode && session.start === trigger.start;
    if (!same) {
      setFilter(null);
      setActiveKey(null);
    }
    if (trigger.mode === "mention") {
      if (same && session.mode === "mention" && session.query !== trigger.query) setActiveKey(null);
      setSession({ mode: "mention", start: trigger.start, query: trigger.query, now: config.now() });
    } else {
      if (same && session.mode === "template" && session.query !== trigger.query) setActiveKey(null);
      setSession({ mode: "template", start: 0, end: caret, query: trigger.query });
    }
  };

  const onInput = (event: ChangeEvent<HTMLTextAreaElement>) => {
    const next = event.target.value;
    const caret = event.target.selectionStart ?? next.length;
    expectedValue.current = next;
    setMentions(reconcileMentions(live, value, next));
    setInserted(null);
    // Typing over a selected blank ends the blank.
    if (session?.mode === "slot") setSession(null);
    onChange(next);
    detect(next, caret);
  };

  const insertEntry = (entry: Entry) => {
    if (session === null || session.mode === "template") return;
    const end = session.mode === "slot" ? session.end : selectionOf().end;
    const edit = mentionInsertion(value, session.start, end, entry.name);
    const shifted = applyEdit(live, session.start, end, edit.text.length - value.length + (end - session.start));
    const mention: Mention = {
      start: edit.labelStart,
      end: edit.labelEnd,
      label: `@${entry.name}`,
      target: entry.target,
    };
    const nextMentions = [...shifted, mention].sort((a, b) => a.start - b.start);
    setRecents((current) => rememberRecent(user, tenantId, entry.key, current));
    setInserted(t("chat.mention.inserted", { name: entry.name }));
    const remaining = session.mode === "slot" ? findSlots(edit.text) : [];
    const next = remaining.find((slot) => slot.start >= edit.caret) ?? remaining[0];
    if (next) {
      goToSlot(next, edit.text, nextMentions);
      return;
    }
    commit({ text: edit.text, mentions: nextMentions, selection: [edit.caret, edit.caret], session: null });
  };

  const insertTemplate = (template: QuestionTemplate) => {
    if (session === null || session.mode !== "template") return;
    for (const slot of template.slots) slotKinds.current.set(slot.label, slot.kind);
    const text = value.slice(0, session.start) + template.text + value.slice(session.end);
    const shifted = applyEdit(live, session.start, session.end, template.text.length);
    const first = findSlots(text).find((slot) => slot.start >= session.start);
    if (first) {
      goToSlot(first, text, shifted);
      return;
    }
    const caret = session.start + template.text.length;
    commit({ text, mentions: shifted, selection: [caret, caret], session: null });
  };

  const choose = (row: Row | undefined) => {
    if (row === undefined) return;
    if (row.template) insertTemplate(row.template);
    else if (row.entry) insertEntry(row.entry);
  };

  const dismiss = () => {
    if (session === null) return;
    if (session.mode === "slot") {
      boxRef.current?.setSelectionRange(session.end, session.end);
    } else {
      dismissed.current = { mode: session.mode, start: session.start };
    }
    setSession(null);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>): boolean => {
    // An input method's composition owns its keys: Enter confirms a word, arrows pick a candidate.
    if (event.nativeEvent.isComposing || event.keyCode === 229) return false;
    if (session !== null) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        if (rows.length === 0) return false;
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        setActiveKey(rows[(activeIndex + step + rows.length) % rows.length]?.key ?? null);
        return true;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        dismiss();
        return true;
      }
      const inserts = event.key === "Enter" || (event.key === "Tab" && session.mode !== "slot");
      if (inserts && !event.shiftKey) {
        event.preventDefault();
        choose(rows[activeIndex]);
        return true;
      }
    }
    if (event.key === "Tab" && slots.length > 0) {
      const { start, end } = selectionOf();
      const slot = neighbourSlot(slots, start, end, event.shiftKey);
      // No blank further on: Tab leaves the box as it always does.
      if (slot === null) return false;
      event.preventDefault();
      goToSlot(slot, value, live);
      return true;
    }
    return false;
  };

  const redetect = () => {
    const box = boxRef.current;
    if (box === null || session?.mode === "slot") return;
    detect(box.value, box.selectionStart ?? box.value.length);
  };

  const insertAtCaret = (text: string, nextSession: (at: number) => Session) => {
    const box = boxRef.current;
    if (box === null) return;
    box.focus();
    const { start, end } = selectionOf();
    const before = value.slice(0, start);
    const lead = text !== "/" && before !== "" && !/\s$/u.test(before) ? " " : "";
    const written = `${lead}${text}`;
    const next = before + written + value.slice(end);
    const shifted = applyEdit(live, start, end, written.length);
    const caret = start + written.length;
    commit({ text: next, mentions: shifted, selection: [caret, caret], session: nextSession(start + lead.length) });
  };

  const openMentions = () => {
    insertAtCaret("@", (at) => ({ mode: "mention", start: at, query: "", now: config.now() }));
  };

  const openTemplates = () => {
    if (templateSource === null) return;
    const box = boxRef.current;
    if (box === null) return;
    box.focus();
    if (value === "") {
      // An empty box gets the `/`, so typing after it narrows the list like the shortcut does.
      insertAtCaret("/", () => ({ mode: "template", start: 0, end: 1, query: "" }));
      return;
    }
    const { start, end } = selectionOf();
    setSession({ mode: "template", start, end, query: "" });
    setActiveKey(null);
  };

  // The list never takes focus (its mouse events do not move it), so leaving the box closes it.
  const onBlur = () => setSession(null);

  const onScroll = () => {
    const mirror = mirrorRef.current;
    const box = boxRef.current;
    if (mirror && box) mirror.scrollTop = box.scrollTop;
  };

  const slotLabel = session?.mode === "slot" ? session.label : "";
  const popover: PopoverModel | null =
    session === null
      ? null
      : {
          listId,
          optionId,
          label:
            session.mode === "template"
              ? t("chat.template.list_label")
              : session.mode === "slot"
                ? t("chat.template.slot_list_label", { slot: SLOT_BRACKETS(slotLabel) })
                : query === ""
                  ? t("chat.mention.list_label")
                  : t("chat.mention.list_label_query", { query }),
          heading:
            session.mode === "slot"
              ? t("chat.template.slot_heading", { slot: SLOT_BRACKETS(slotLabel), count: slots.length })
              : null,
          filtersLabel: t("chat.mention.filters"),
          filters:
            session.mode === "mention"
              ? ([null, "device", "point", "model", "range"] as const).map((candidate) => ({
                  kind: candidate,
                  label: t(`chat.mention.filter.${candidate ?? "all"}`),
                  pressed: filter === candidate,
                }))
              : null,
          groups,
          activeKey: rows[activeIndex]?.key ?? null,
          highlight: query,
          skeleton,
          error: failed
            ? session.mode === "template"
              ? { title: t("chat.template.error_title"), body: t("chat.template.error_body") }
              : { title: t("chat.mention.error_title"), body: t("chat.mention.error_body") }
            : null,
          empty:
            rows.length === 0 && !loading && !failed
              ? session.mode === "template"
                ? { title: t("chat.template.none_title", { query: session.query }), body: t("chat.template.none_body") }
                : query !== ""
                  ? {
                      title: t("chat.mention.none_title", { query }),
                      body: t("chat.mention.none_body", {
                        tenant: tenantName ?? t("chat.mention.only_tenant_unnamed"),
                      }),
                    }
                  : {
                      title: t("chat.mention.none_title_empty"),
                      body: t("chat.mention.none_body_empty", {
                        tenant: tenantName ?? t("chat.mention.only_tenant_unnamed"),
                      }),
                    }
              : null,
          retryLabel: t("chat.mention.retry"),
          tenantNote: tenantName
            ? t("chat.mention.only_tenant", { tenant: tenantName })
            : t("chat.mention.only_tenant_unnamed"),
          keys: {
            move: t("chat.mention.key_move"),
            insert: t("chat.mention.key_insert"),
            close: t("chat.mention.key_close"),
            nextBlank: session.mode === "slot" ? t("chat.mention.key_next_blank") : null,
          },
          placement,
          onChoose: (row) => {
            choose(row);
            boxRef.current?.focus();
          },
          onHover: (key) => setActiveKey(key),
          onFilter: (next) => {
            setFilter(next);
            setActiveKey(null);
            boxRef.current?.focus();
          },
          onRetry: session.mode === "template" ? () => void templatesQuery.refetch() : sources.retry,
        };

  const selectedSlot = session?.mode === "slot" ? ([session.start, session.end] satisfies [number, number]) : null;
  const queryRange =
    session?.mode === "mention"
      ? ([session.start, session.start + 1 + session.query.length] satisfies [number, number])
      : null;

  return {
    boxRef,
    mirrorRef,
    textareaProps: {
      onChange: onInput,
      onClick: redetect,
      onKeyUp: (event) => {
        if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) redetect();
      },
      onBlur,
      onScroll,
      "aria-autocomplete": "list",
      "aria-controls": popover ? listId : undefined,
      "aria-activedescendant": popover && rows.length > 0 ? optionId(activeIndex) : undefined,
    },
    onKeyDown,
    mirror: { mentions: live, slotSelection: selectedSlot, query: queryRange },
    popover,
    mentions: live.map(({ start, end, label, target }) => ({ start, end, label, target })),
    slotsLeft: slots.length,
    notice: slots.length > 0 ? t("chat.template.slots_left", { count: slots.length }) : null,
    templatesOffered: templateSource !== null,
    openMentions,
    openTemplates,
    liveText: open ? settledCount : inserted,
    helpId,
  };
}
