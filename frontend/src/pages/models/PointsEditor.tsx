import { ArrowDownIcon, ArrowUpIcon, PlusIcon, Trash2Icon } from "lucide-react";
import { type MutableRefObject, type ReactNode, useId } from "react";

import { Button } from "../../components/ui/button.tsx";
import { Input } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { useMediaQuery } from "../../lib/use-media-query.ts";
import { cn } from "../../lib/utils.ts";
import { type Problem, problemText } from "../form-problems.ts";
import {
  blankPoint,
  byteOrdersFor,
  dataTypesFor,
  FUNCTION_CODES,
  isNumeric,
  manualAddress,
  type PointDraft,
  type PointField,
  pointIndexOf,
} from "./modelForm.ts";

/** What an edit tells the page: which problems the edit answers. */
export type PointsChange = { clear: readonly string[] } | { clearAll: true };

const FUNCTION_LABELS: Readonly<Record<string, MessageId>> = {
  "1": "model_edit.function.1",
  "2": "model_edit.function.2",
  "3": "model_edit.function.3",
  "4": "model_edit.function.4",
};

type Draft = PointDraft;

/** Which draft member each field of the API's data point is. */
const MEMBER: Readonly<Record<PointField, keyof Draft>> = {
  key: "key",
  function_code: "functionCode",
  address: "address",
  data_type: "dataType",
  byte_order: "byteOrder",
  length: "length",
  scale: "scale",
  offset: "offset",
  description: "description",
};

const ARIA: Readonly<Record<PointField, MessageId>> = {
  key: "model_edit.aria.key",
  function_code: "model_edit.aria.function_code",
  address: "model_edit.aria.address",
  data_type: "model_edit.aria.data_type",
  byte_order: "model_edit.aria.byte_order",
  length: "model_edit.aria.length",
  scale: "model_edit.aria.scale",
  offset: "model_edit.aria.offset",
  description: "model_edit.aria.description",
};

/**
 * The data points of a model as docs/design/tenant-admin/ draws them (`devices--model-edit`): an
 * editable table on a wide screen, an expandable card per data point on a phone (one form at a
 * time: `useMediaQuery`, so a screen reader and a test find each input once). Rows move up and
 * down and go; a row with a problem is marked and, on a phone, open. Without `onChange` (a
 * viewer) every input is read-only.
 */
export function PointsEditor(props: {
  drafts: readonly Draft[];
  onChange?: (next: Draft[], change: PointsChange) => void;
  nextId: () => number;
  problems: readonly Problem[];
  /** Where the inputs register themselves by pointer, so the page's summary can focus one. */
  inputs: MutableRefObject<Map<string, HTMLElement | null>>;
}) {
  const { t, locale } = useI18n();
  const wide = useMediaQuery("(min-width: 768px)");
  const readOnly = props.onChange === undefined;
  const { drafts, problems } = props;
  const errorsId = useId();

  const edit = (index: number, patch: Partial<Draft>, pointers: readonly string[]) => {
    props.onChange?.(
      drafts.map((draft, at) => (at === index ? { ...draft, ...patch } : draft)),
      { clear: pointers.map((pointer) => `/points/${index}/${pointer}`) },
    );
  };
  const change = (index: number, field: PointField, value: string) => {
    const patch: Partial<Draft> = { [MEMBER[field]]: value };
    const draft = drafts[index] as Draft;
    const cleared: string[] = [field];
    // A function code that reads bits is for bool only; a type that is no number has no scaling.
    if (field === "function_code") {
      const types = dataTypesFor(value);
      if (!types.includes(draft.dataType)) {
        patch.dataType = "bool";
        cleared.push("data_type", "byte_order", "length", "scale", "offset");
      }
    }
    if (field === "data_type") {
      const dataType = value as Draft["dataType"];
      if (!byteOrdersFor(dataType).includes(draft.byteOrder)) patch.byteOrder = "abcd";
      cleared.push("byte_order", "length", "scale", "offset");
    }
    edit(index, patch, cleared);
  };
  const move = (index: number, by: -1 | 1) => {
    const next = [...drafts];
    const [moved] = next.splice(index, 1);
    next.splice(index + by, 0, moved as Draft);
    props.onChange?.(next, { clearAll: true });
  };
  const remove = (index: number) => {
    props.onChange?.(
      drafts.filter((_, at) => at !== index),
      { clearAll: true },
    );
  };
  const add = () => {
    props.onChange?.([...drafts, blankPoint(props.nextId())], { clearAll: true });
  };

  const problemsOf = (index: number) => problems.filter((problem) => pointIndexOf(problem.pointer) === index);
  const hasProblem = (index: number, field: PointField) =>
    problems.some((problem) => problem.pointer === `/points/${index}/${field}`);
  const register = (index: number, field: PointField) => (element: HTMLElement | null) => {
    props.inputs.current.set(`/points/${index}/${field}`, element);
  };

  /** One input of one data point: the same controls in both layouts. */
  const control = (index: number, field: PointField): ReactNode => {
    const draft = drafts[index] as Draft;
    const value = String(draft[MEMBER[field]]);
    const common = {
      "aria-label": t(ARIA[field], { n: index + 1 }),
      "aria-invalid": hasProblem(index, field) ? (true as const) : undefined,
      "aria-describedby": problemsOf(index).length > 0 ? `${errorsId}-${draft.id}` : undefined,
      ref: register(index, field),
    };
    switch (field) {
      case "function_code":
        return (
          <NativeSelect
            {...common}
            value={value}
            disabled={readOnly}
            onChange={(event) => change(index, field, event.target.value)}
          >
            {FUNCTION_CODES.map((code) => (
              <option key={code} value={String(code)}>
                {t(FUNCTION_LABELS[String(code)] as MessageId)}
              </option>
            ))}
          </NativeSelect>
        );
      case "data_type":
        return (
          <NativeSelect
            {...common}
            value={value}
            disabled={readOnly}
            onChange={(event) => change(index, field, event.target.value)}
          >
            {dataTypesFor(draft.functionCode).map((type) => (
              <option key={type} value={type}>
                {type}
              </option>
            ))}
          </NativeSelect>
        );
      case "byte_order":
        return byteOrdersFor(draft.dataType).length === 0 ? (
          <span className="text-sm text-muted-foreground">{t("model_edit.not_applicable")}</span>
        ) : (
          <NativeSelect
            {...common}
            value={value}
            disabled={readOnly}
            onChange={(event) => change(index, field, event.target.value)}
          >
            {byteOrdersFor(draft.dataType).map((order) => (
              <option key={order} value={order}>
                {order}
              </option>
            ))}
          </NativeSelect>
        );
      case "length":
        return draft.dataType === "string" ? (
          <Input
            {...common}
            inputMode="numeric"
            value={value}
            readOnly={readOnly}
            onChange={(event) => change(index, field, event.target.value)}
          />
        ) : (
          <span className="text-sm text-muted-foreground">—</span>
        );
      case "scale":
      case "offset":
        return (
          <Input
            {...common}
            inputMode="decimal"
            placeholder={field === "scale" ? "1" : "0"}
            value={value}
            readOnly={readOnly}
            onChange={(event) => change(index, field, event.target.value)}
          />
        );
      default:
        return (
          <Input
            {...common}
            value={value}
            readOnly={readOnly}
            autoComplete="off"
            spellCheck={false}
            onChange={(event) => change(index, field, event.target.value)}
          />
        );
    }
  };

  const address = (index: number) => {
    const draft = drafts[index] as Draft;
    const manual = manualAddress(draft.functionCode, draft.address);
    return (
      <div className="grid gap-1">
        {control(index, "address")}
        {manual ? (
          <span className="text-xs text-muted-foreground">{t("model_edit.manual_address", { address: manual })}</span>
        ) : null}
      </div>
    );
  };
  const scaling = (index: number) => {
    const draft = drafts[index] as Draft;
    if (!isNumeric(draft.dataType))
      return <span className="text-sm text-muted-foreground">{t("model_edit.not_scalable")}</span>;
    return (
      <div className="flex items-center gap-1">
        <div className="w-20">{control(index, "scale")}</div>
        <span aria-hidden="true">+</span>
        <div className="w-20">{control(index, "offset")}</div>
      </div>
    );
  };

  const errorRow = (index: number) => {
    const draft = drafts[index] as Draft;
    const own = problemsOf(index);
    if (own.length === 0) return null;
    return (
      <ul id={`${errorsId}-${draft.id}`} className="grid gap-0.5 text-sm font-medium text-destructive">
        {own.map((problem) => (
          <li key={`${problem.pointer}|${problem.message_key}`}>{problemText(locale, problem)}</li>
        ))}
      </ul>
    );
  };

  const actions = (index: number) => {
    const draft = drafts[index] as Draft;
    const label = draft.key.trim() === "" ? "" : ` ${draft.key.trim()}`;
    return (
      <div className="flex items-center gap-0.5">
        <Button
          variant="ghost"
          size="icon"
          aria-label={t("model_edit.move_up", { n: index + 1 })}
          disabled={index === 0}
          onClick={() => move(index, -1)}
        >
          <ArrowUpIcon aria-hidden="true" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          aria-label={t("model_edit.move_down", { n: index + 1 })}
          disabled={index === drafts.length - 1}
          onClick={() => move(index, 1)}
        >
          <ArrowDownIcon aria-hidden="true" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          aria-label={t("model_edit.remove_point", { n: index + 1, key: label })}
          className="text-destructive"
          onClick={() => remove(index)}
        >
          <Trash2Icon aria-hidden="true" />
        </Button>
      </div>
    );
  };

  const columns: MessageId[] = [
    "model_edit.column.number",
    "model_edit.column.key",
    "model_edit.column.function",
    "model_edit.column.address",
    "model_edit.column.type",
    "model_edit.column.byte_order",
    "model_edit.column.length",
    "model_edit.column.scaling",
    "model_edit.column.description",
  ];

  return (
    <div className="grid gap-3">
      {readOnly ? null : (
        <div>
          <Button variant="outline" size="sm" onClick={add}>
            <PlusIcon aria-hidden="true" />
            {t("model_edit.add_point")}
          </Button>
        </div>
      )}
      {drafts.length === 0 ? (
        <p className="rounded-lg border border-dashed px-4 py-6 text-sm text-muted-foreground">
          {t("model_edit.no_points")}
        </p>
      ) : wide ? (
        <div className="overflow-x-auto rounded-lg border">
          <table className="w-full min-w-[60rem] text-sm">
            <caption className="sr-only">{t("model_edit.points_caption")}</caption>
            <thead className="bg-muted/60 text-left text-muted-foreground">
              <tr>
                {columns.map((column) => (
                  <th key={column} scope="col" className="px-2 py-2 font-medium whitespace-nowrap">
                    {t(column)}
                  </th>
                ))}
                {readOnly ? null : (
                  <th scope="col" className="px-2 py-2 font-medium">
                    {t("model_edit.column.actions")}
                  </th>
                )}
              </tr>
            </thead>
            <tbody>
              {drafts.map((draft, index) => {
                const own = problemsOf(index);
                return (
                  <tr
                    key={draft.id}
                    aria-invalid={own.length > 0 ? true : undefined}
                    className={cn(
                      "border-t align-top",
                      own.length > 0 && "border-l-4 border-l-destructive bg-destructive/5",
                    )}
                  >
                    <td className="px-2 py-2 text-muted-foreground">{index + 1}</td>
                    <td className="min-w-36 px-2 py-2">{control(index, "key")}</td>
                    <td className="min-w-40 px-2 py-2">{control(index, "function_code")}</td>
                    <td className="min-w-28 px-2 py-2">{address(index)}</td>
                    <td className="min-w-24 px-2 py-2">{control(index, "data_type")}</td>
                    <td className="min-w-24 px-2 py-2">{control(index, "byte_order")}</td>
                    <td className="min-w-16 px-2 py-2">{control(index, "length")}</td>
                    <td className="px-2 py-2">{scaling(index)}</td>
                    <td className="min-w-48 px-2 py-2">
                      <div className="grid gap-1">
                        {control(index, "description")}
                        {errorRow(index)}
                      </div>
                    </td>
                    {readOnly ? null : <td className="px-2 py-1">{actions(index)}</td>}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="grid gap-2">
          {drafts.map((draft, index) => {
            const own = problemsOf(index);
            return (
              <details
                key={draft.id}
                open={own.length > 0 || (index === 0 && drafts.length === 1) || undefined}
                className={cn("rounded-lg border bg-card", own.length > 0 && "border-l-4 border-l-destructive")}
              >
                <summary className="cursor-pointer px-3 py-2.5 text-sm font-medium">
                  {t("model_edit.card_summary", {
                    n: index + 1,
                    key: draft.key.trim() === "" ? t("model_edit.card_unnamed") : draft.key.trim(),
                    code: draft.functionCode,
                    address: draft.address,
                    type: draft.dataType,
                  })}
                </summary>
                <div className="grid gap-3 px-3 pb-3">
                  <CardField label={t("model_edit.column.key")}>{control(index, "key")}</CardField>
                  <CardField label={t("model_edit.column.function")}>{control(index, "function_code")}</CardField>
                  <CardField label={t("model_edit.column.address")}>{address(index)}</CardField>
                  <CardField label={t("model_edit.column.type")}>{control(index, "data_type")}</CardField>
                  {byteOrdersFor(draft.dataType).length > 0 ? (
                    <CardField label={t("model_edit.column.byte_order")}>{control(index, "byte_order")}</CardField>
                  ) : null}
                  {draft.dataType === "string" ? (
                    <CardField label={t("model_edit.column.length")}>{control(index, "length")}</CardField>
                  ) : null}
                  <CardField label={t("model_edit.column.scaling")}>{scaling(index)}</CardField>
                  <CardField label={t("model_edit.column.description")}>{control(index, "description")}</CardField>
                  {errorRow(index)}
                  {readOnly ? null : actions(index)}
                </div>
              </details>
            );
          })}
        </div>
      )}
    </div>
  );
}

function CardField({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid gap-1">
      <span className="text-xs font-medium text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}
