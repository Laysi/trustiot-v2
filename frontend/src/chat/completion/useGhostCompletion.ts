import {
  type FocusEvent,
  type KeyboardEvent,
  type MouseEvent,
  type Ref,
  type RefCallback,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { ApiError } from "../../api/errors.ts";
import { useCompletionSettings } from "./context.tsx";

/** The design asks for at least 4 characters (docs/design/home-ai/design.md, 「LLM 續寫」). */
export const MIN_COMPLETION_CHARS = 4;

/** Used when a 429 does not say when to come back. */
const DEFAULT_RETRY_AFTER_SECS = 60;

const MODIFIER_KEYS: ReadonlySet<string> = new Set(["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]);

/** A suggestion and the text it continues; the mirror layer draws the two together. */
export interface Suggestion {
  base: string;
  text: string;
}

export interface GhostCompletion {
  /** Whether this build has completion at all; when not, nothing below does anything. */
  active: boolean;
  /** What is shown after the caret, or `null`. */
  suggestion: Suggestion | null;
  /** Set while the per-user limit applies: about how many minutes until suggestions return. */
  pausedMinutes: number | null;
  /** The last suggestion was just accepted (a polite live region says so once). */
  accepted: boolean;
  /**
   * Call first in the box's key handler. `true` means the key was Tab or Esc and was used here,
   * so the caller does nothing else with it. Any other key cancels the request in flight.
   */
  handleKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => boolean;
  accept: () => void;
  /** Spread on the `textarea`: the merged ref and the handlers that cancel a suggestion. */
  textareaProps: {
    ref: RefCallback<HTMLTextAreaElement>;
    onSelect: () => void;
    onClick: () => void;
    onBlur: (event: FocusEvent<HTMLTextAreaElement>) => void;
    onCompositionStart: () => void;
    onCompositionEnd: () => void;
  };
  /** Spread on the mobile accept button: it must not take the focus from the box. */
  acceptButtonProps: {
    ref: RefCallback<HTMLButtonElement>;
    onMouseDown: (event: MouseEvent<HTMLButtonElement>) => void;
    onClick: () => void;
  };
}

function assign<T>(ref: Ref<T> | undefined, value: T | null) {
  if (typeof ref === "function") ref(value);
  else if (ref) ref.current = value;
}

function charCount(text: string): number {
  return Array.from(text.trim()).length;
}

/** How the server says the per-user limit applies: seconds until the next request is allowed. */
function limitedFor(error: unknown): number | null {
  if (!(error instanceof ApiError)) return null;
  const code = error.body?.code;
  if (code !== "rate_limited" && !(code === undefined && error.status === 429)) return null;
  const advised = error.body?.details?.retry_after_secs;
  return typeof advised === "number" && advised > 0 ? advised : DEFAULT_RETRY_AFTER_SECS;
}

/**
 * Grey-text completion for a composer's box (J11, Laysi/trustiot-v2#156). While the user stops
 * typing for the configured pause, with the caret at the end and nothing else going on, it asks
 * for a continuation and offers it; Tab accepts it. What cancels it:
 *
 * - any other key press aborts the request in flight and clears the suggestion, and a response
 *   that arrives afterwards is dropped (a token per request, not only the signal, because a
 *   service may not honour the abort);
 * - Esc: no request until the text changes again;
 * - a click or selection away from the end, losing focus, an input method's composition, a
 *   `paused` or `disabled` box.
 *
 * Once the server answers 429, no request is sent until the retry time has passed, and the
 * toolbar says so once. Other failures show nothing: the user did not ask for the suggestion.
 * Without a `CompletionProvider` the hook does nothing, so the product's composer shows no grey
 * text until the endpoint exists.
 */
export function useGhostCompletion({
  value,
  disabled,
  paused = false,
  textareaRef,
}: {
  value: string;
  /** The box cannot be typed in (a question is being sent or answered). */
  disabled: boolean;
  /**
   * Something else owns the keys for now: an open mention list or an unfilled template slot.
   * Tab never goes to the suggestion then (the order is mentions, slots, grey text, focus).
   */
  paused?: boolean;
  /** The caller's own ref to the box, kept working. */
  textareaRef?: Ref<HTMLTextAreaElement> | undefined;
}): GhostCompletion {
  const settings = useCompletionSettings();
  const [suggestion, setSuggestionState] = useState<Suggestion | null>(null);
  const [pausedMinutes, setPausedMinutes] = useState<number | null>(null);
  const [accepted, setAccepted] = useState(false);

  const box = useRef<HTMLTextAreaElement | null>(null);
  const acceptButton = useRef<HTMLButtonElement | null>(null);
  const shown = useRef<Suggestion | null>(null);
  const latest = useRef({ disabled, paused, settings });
  const run = useRef({
    timer: undefined as unknown,
    limitTimer: undefined as unknown,
    controller: null as AbortController | null,
    token: 0,
    composing: false,
    /** Asking is off while the text is exactly this (after Esc or an accepted suggestion). */
    suppressedFor: null as string | null,
    limitedUntil: null as number | null,
  });

  useLayoutEffect(() => {
    latest.current = { disabled, paused, settings };
  });

  const setSuggestion = useCallback((next: Suggestion | null) => {
    shown.current = next;
    setSuggestionState(next);
  }, []);

  /** Drops the pending timer, aborts the request in flight and clears the suggestion. */
  const cancel = useCallback(() => {
    const state = run.current;
    if (state.timer !== undefined) {
      latest.current.settings?.clock.clearTimeout(state.timer);
      state.timer = undefined;
    }
    state.controller?.abort();
    state.controller = null;
    state.token += 1;
    setSuggestion(null);
  }, [setSuggestion]);

  const askable = useCallback((): HTMLTextAreaElement | null => {
    const { disabled: off, paused: other } = latest.current;
    const element = box.current;
    const state = run.current;
    if (!latest.current.settings || off || other || state.composing || element === null) return null;
    if (state.suppressedFor === element.value) return null;
    if (element.ownerDocument.activeElement !== element) return null;
    const end = element.value.length;
    if (element.selectionStart !== end || element.selectionEnd !== end) return null;
    return charCount(element.value) >= MIN_COMPLETION_CHARS ? element : null;
  }, []);

  const startLimit = useCallback((seconds: number) => {
    const current = latest.current.settings;
    if (!current) return;
    const state = run.current;
    state.limitedUntil = current.clock.now() + seconds * 1000;
    setPausedMinutes(Math.max(1, Math.ceil(seconds / 60)));
    if (state.limitTimer !== undefined) current.clock.clearTimeout(state.limitTimer);
    state.limitTimer = current.clock.setTimeout(() => {
      state.limitTimer = undefined;
      state.limitedUntil = null;
      setPausedMinutes(null);
    }, seconds * 1000);
  }, []);

  const send = useCallback(() => {
    const current = latest.current.settings;
    const element = askable();
    const state = run.current;
    if (!current || element === null) return;
    if (state.limitedUntil !== null && current.clock.now() < state.limitedUntil) return;
    const base = element.value;
    const controller = new AbortController();
    state.controller = controller;
    const token = state.token;
    const stale = () => token !== state.token || controller.signal.aborted;
    current.complete(base, controller.signal).then(
      (text) => {
        if (stale()) return;
        state.controller = null;
        if (text.trim() === "") return;
        setAccepted(false);
        setSuggestion({ base, text });
      },
      (error: unknown) => {
        if (stale()) return;
        state.controller = null;
        const seconds = limitedFor(error);
        if (seconds !== null) startLimit(seconds);
      },
    );
  }, [askable, setSuggestion, startLimit]);

  const schedule = useCallback(() => {
    const current = latest.current.settings;
    const state = run.current;
    if (!current || askable() === null) return;
    state.timer = current.clock.setTimeout(() => {
      state.timer = undefined;
      send();
    }, current.delayMs);
  }, [askable, send]);

  // A change of the text, or of whether the box may be asked, starts over: the suggestion was for
  // other text, and the pause counts from the last change.
  // biome-ignore lint/correctness/useExhaustiveDependencies: `value`, `disabled` and `paused` are the triggers; the callbacks read them through refs.
  useEffect(() => {
    if (accepted && run.current.suppressedFor !== value) setAccepted(false);
    schedule();
    return cancel;
  }, [value, disabled, paused, settings, schedule, cancel]);

  useEffect(
    () => () => {
      const state = run.current;
      const current = latest.current.settings;
      if (current && state.limitTimer !== undefined) current.clock.clearTimeout(state.limitTimer);
    },
    [],
  );

  const accept = useCallback(() => {
    const element = box.current;
    const offered = shown.current;
    if (element === null || offered === null || element.value !== offered.base) return;
    const end = element.value.length;
    run.current.suppressedFor = offered.base + offered.text;
    cancel();
    element.focus();
    element.setSelectionRange(end, end);
    // `insertText` keeps the browser's undo stack, so one Ctrl/Cmd + Z takes the whole suggestion
    // back. Where the editing command is missing or refuses, set the value through the prototype's
    // setter (React's own setter on the element would hide the change from `onChange`) and send the
    // `input` event a keystroke would have.
    const inserted =
      typeof document.execCommand === "function" && document.execCommand("insertText", false, offered.text);
    if (!inserted) {
      const setValue = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
      setValue?.call(element, offered.base + offered.text);
      const caret = element.value.length;
      element.setSelectionRange(caret, caret);
      element.dispatchEvent(new Event("input", { bubbles: true }));
    }
    setAccepted(true);
  }, [cancel]);

  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLTextAreaElement>): boolean => {
      if (!latest.current.settings) return false;
      if (event.nativeEvent.isComposing || event.keyCode === 229) {
        cancel();
        return false;
      }
      if (MODIFIER_KEYS.has(event.key)) return false;
      const plainTab = event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey;
      if (plainTab && shown.current !== null && !latest.current.paused && !event.defaultPrevented) {
        event.preventDefault();
        accept();
        return true;
      }
      if (event.key === "Escape") {
        const wasShown = shown.current !== null;
        run.current.suppressedFor = event.currentTarget.value;
        cancel();
        if (wasShown) event.preventDefault();
        return wasShown;
      }
      cancel();
      return false;
    },
    [accept, cancel],
  );

  const awayFromEnd = useCallback(() => {
    const element = box.current;
    if (element === null) return;
    const end = element.value.length;
    if (element.selectionStart !== end || element.selectionEnd !== end) cancel();
  }, [cancel]);

  const textareaRefCallback = useCallback(
    (element: HTMLTextAreaElement | null) => {
      box.current = element;
      assign(textareaRef, element);
    },
    [textareaRef],
  );

  const acceptButtonRef = useCallback((element: HTMLButtonElement | null) => {
    acceptButton.current = element;
  }, []);

  return useMemo<GhostCompletion>(
    () => ({
      active: settings !== null,
      suggestion,
      pausedMinutes,
      accepted,
      handleKeyDown,
      accept,
      textareaProps: {
        ref: textareaRefCallback,
        onSelect: awayFromEnd,
        onClick: awayFromEnd,
        onBlur: (event) => {
          // Pressing the accept button moves the focus to it before it is clicked.
          if (event.relatedTarget !== null && event.relatedTarget === acceptButton.current) return;
          cancel();
        },
        onCompositionStart: () => {
          run.current.composing = true;
          cancel();
        },
        onCompositionEnd: () => {
          run.current.composing = false;
          schedule();
        },
      },
      acceptButtonProps: {
        ref: acceptButtonRef,
        // Keeps the focus (and the selection) in the box, so the suggestion is not cleared first.
        onMouseDown: (event) => event.preventDefault(),
        onClick: accept,
      },
    }),
    [
      settings,
      suggestion,
      pausedMinutes,
      accepted,
      handleKeyDown,
      accept,
      textareaRefCallback,
      awayFromEnd,
      cancel,
      schedule,
      acceptButtonRef,
    ],
  );
}
