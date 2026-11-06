import type { Entry, SourceStatus, Sources } from "./sources.ts";
import type { QuestionTemplate } from "./templates.tsx";
import { type MentionKind, rank } from "./text.ts";

// What the open list shows, as groups of rows (design 04, 05 and 06). Built from the sources, the
// time ranges, the recents and the templates by pure rules, so the ranking and the limits are
// tested without a DOM. The rules the design leaves open are fixed here, and written down in
// design.md (「和設計稿不同的地方」): at most 5 rows per group while the list is not narrowed to one
// kind, 50 once it is; matches are ranked prefix first, then by name.

/** Rows per group while every kind is listed, and once the list is narrowed to one kind or a blank. */
export const GROUP_LIMIT = 5;
export const NARROWED_LIMIT = 50;

export interface Row {
  key: string;
  kind: MentionKind | "template";
  name: string;
  meta: string;
  /** For a mention: what it stands for. */
  entry?: Entry;
  /** For a template. */
  template?: QuestionTemplate;
}

export interface Group {
  id: string;
  title: string;
  rows: Row[];
  /** How many matched; `rows` may be fewer (the limit). */
  total: number;
  /** The lookup behind the group is still running and there is nothing from it to show yet. */
  loading: boolean;
}

const entryRow = (entry: Entry): Row => ({
  key: entry.key,
  kind: entry.kind,
  name: entry.name,
  meta: entry.meta,
  entry,
});

export interface MentionListInput {
  query: string;
  /** The kind chosen with the filter buttons, or the kind of the blank being filled; `null` lists every kind. */
  kind: MentionKind | null;
  sources: Sources;
  ranges: Entry[];
  /** The keys last mentioned, newest first. */
  recents: readonly string[];
  titles: Record<MentionKind | "recent", string>;
}

function pick(
  id: MentionKind,
  title: string,
  entries: readonly Entry[],
  status: SourceStatus,
  query: string,
  limit: number,
): Group {
  const matches = rank(entries, query);
  return {
    id,
    title,
    rows: matches.slice(0, limit).map(entryRow),
    total: matches.length,
    loading: status === "loading" && entries.length === 0,
  };
}

/** The groups of the `@` list. Groups with nothing to show and nothing loading are left out. */
export function mentionGroups(input: MentionListInput): Group[] {
  const { query, kind, sources, ranges, recents, titles } = input;
  const limit = kind === null ? GROUP_LIMIT : NARROWED_LIMIT;
  const groups: Group[] = [];
  // Only `@`: the last used first, then the devices, and the time ranges (the points and models wait for a word).
  if (query === "" && kind === null) {
    const known = new Map([...sources.devices, ...sources.models, ...sources.points, ...ranges].map((e) => [e.key, e]));
    const recent = recents.flatMap((key) => known.get(key) ?? []);
    if (recent.length > 0) {
      groups.push({
        id: "recent",
        title: titles.recent,
        rows: recent.map(entryRow),
        total: recent.length,
        loading: false,
      });
    }
    const recentKeys = new Set(recent.map((entry) => entry.key));
    const without = (entries: Entry[]) => entries.filter((entry) => !recentKeys.has(entry.key));
    groups.push(pick("device", titles.device, without(sources.devices), sources.status.devices, "", limit));
    groups.push(pick("range", titles.range, without(ranges), "ready", "", limit));
  } else {
    if (kind === null || kind === "device") {
      groups.push(pick("device", titles.device, sources.devices, sources.status.devices, query, limit));
    }
    if (kind === null || kind === "point") {
      groups.push(pick("point", titles.point, sources.points, sources.status.points, query, limit));
    }
    if (kind === null || kind === "model") {
      groups.push(pick("model", titles.model, sources.models, sources.status.models, query, limit));
    }
    if (kind === null || kind === "range") groups.push(pick("range", titles.range, ranges, "ready", query, limit));
  }
  return groups.filter((group) => group.rows.length > 0 || group.loading);
}

/** The groups of the template list: by the kind of question, matching the text, the heading or the tool. */
export function templateGroups(
  templates: readonly QuestionTemplate[],
  query: string,
  status: SourceStatus,
  loadingTitle: string,
  meta: (template: QuestionTemplate) => string,
): Group[] {
  const needle = query.toLocaleLowerCase();
  const matching = templates.filter(
    (template) =>
      needle === "" ||
      [template.text, template.group, template.tool].some((text) => text.toLocaleLowerCase().includes(needle)),
  );
  const groups: Group[] = [];
  for (const template of matching) {
    let group = groups.find((candidate) => candidate.id === template.group);
    if (group === undefined) {
      group = { id: template.group, title: template.group, rows: [], total: 0, loading: false };
      groups.push(group);
    }
    group.rows.push({
      key: `template:${template.id}`,
      kind: "template",
      name: template.text,
      meta: meta(template),
      template,
    });
    group.total += 1;
  }
  if (status === "loading" && groups.length === 0) {
    return [{ id: "loading", title: loadingTitle, rows: [], total: 0, loading: true }];
  }
  return groups;
}
