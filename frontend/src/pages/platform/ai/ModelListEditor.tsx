import { ArrowDownIcon, ArrowUpIcon, PlusIcon, XIcon } from "lucide-react";
import { useId, useState } from "react";

import type { ModelProvider } from "../../../api/modelRoles.ts";
import { Button } from "../../../components/ui/button.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { Input } from "../../../components/ui/input.tsx";
import { NativeSelect } from "../../../components/ui/native-select.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";

/** The most models a role may have (the AI gateway refuses more). */
export const MAX_MODELS = 8;

/** The part of `<provider>/<model>` after the provider. */
export function modelName(model: string): string {
  const slash = model.indexOf("/");
  return slash < 0 ? model : model.slice(slash + 1);
}

/** A model ID has no spaces and no commas (the check the AI gateway makes, done here first). */
export function isValidModelId(id: string): boolean {
  return id !== "" && !/[\s,]/u.test(id);
}

/**
 * The list a role's models are edited in: up, down and remove buttons on every row (no dragging,
 * so the keyboard can do all of it), and a provider and model ID to add. Providers are the ones
 * that speak the role's protocol. A move is announced as the new position.
 */
export function ModelListEditor({
  models,
  providers,
  onChange,
  label,
}: {
  models: readonly string[];
  /** Only providers of the role's protocol. */
  providers: readonly ModelProvider[];
  onChange: (models: string[]) => void;
  label: string;
}) {
  const { t } = useI18n();
  const [provider, setProvider] = useState(providers[0]?.name ?? "");
  const [id, setId] = useState("");
  const [announcement, setAnnouncement] = useState("");
  const listId = useId();

  const candidate = `${provider}/${id.trim()}`;
  const idError =
    id.trim() === ""
      ? undefined
      : !isValidModelId(id.trim())
        ? t("platform_ai.edit.err_model_id")
        : models.includes(candidate)
          ? t("platform_ai.edit.err_duplicate", { model: candidate })
          : models.length >= MAX_MODELS
            ? t("platform_ai.edit.err_full")
            : undefined;
  const canAdd = provider !== "" && id.trim() !== "" && idError === undefined;

  const move = (index: number, by: -1 | 1) => {
    const next = [...models];
    const [item] = next.splice(index, 1);
    next.splice(index + by, 0, item as string);
    onChange(next);
    setAnnouncement(t("platform_ai.edit.moved", { model: modelName(item as string), position: index + by + 1 }));
  };
  const remove = (index: number) => {
    onChange(models.filter((_, at) => at !== index));
    setAnnouncement(t("platform_ai.edit.removed", { model: modelName(models[index] as string) }));
  };
  const add = () => {
    if (!canAdd) return;
    onChange([...models, candidate]);
    setAnnouncement(t("platform_ai.edit.added", { model: candidate }));
    setId("");
  };

  return (
    <div className="grid gap-4">
      <ol id={listId} aria-label={label} className="grid gap-1.5">
        {models.map((model, index) => (
          <li
            key={model}
            className={cn("flex items-center gap-2 rounded-md border px-3 py-1.5", index === 0 && "border-primary/50")}
          >
            <span aria-hidden="true" className="w-4 text-sm text-muted-foreground tabular-nums">
              {index + 1}
            </span>
            <span className="min-w-0 flex-1 font-mono text-sm wrap-anywhere">{model}</span>
            <span className="flex gap-0.5">
              <Button
                variant="ghost"
                size="icon"
                className="size-8"
                disabled={index === 0}
                aria-label={t("platform_ai.edit.move_up", { model: modelName(model) })}
                onClick={() => move(index, -1)}
              >
                <ArrowUpIcon />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                className="size-8"
                disabled={index === models.length - 1}
                aria-label={t("platform_ai.edit.move_down", { model: modelName(model) })}
                onClick={() => move(index, 1)}
              >
                <ArrowDownIcon />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                className="size-8"
                aria-label={t("platform_ai.edit.remove", { model: modelName(model) })}
                onClick={() => remove(index)}
              >
                <XIcon />
              </Button>
            </span>
          </li>
        ))}
      </ol>
      {models.length === 0 ? (
        <p role="alert" className="text-sm font-medium text-destructive">
          {t("platform_ai.edit.err_none")}
        </p>
      ) : null}
      {providers.length === 0 ? (
        <p className="text-sm text-muted-foreground">{t("platform_ai.edit.no_provider")}</p>
      ) : (
        <div className="grid gap-2 sm:grid-cols-[10rem_1fr_auto] sm:items-start">
          <FormField label={t("platform_ai.edit.provider")}>
            {(control) => (
              <NativeSelect {...control} value={provider} onChange={(event) => setProvider(event.target.value)}>
                {providers.map((item) => (
                  <option key={item.name} value={item.name}>
                    {item.name}
                  </option>
                ))}
              </NativeSelect>
            )}
          </FormField>
          <FormField label={t("platform_ai.edit.model_id")} error={idError}>
            {(control) => (
              <Input
                {...control}
                className="font-mono"
                value={id}
                autoComplete="off"
                spellCheck={false}
                onChange={(event) => setId(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    add();
                  }
                }}
              />
            )}
          </FormField>
          <Button variant="outline" className="sm:mt-[1.625rem]" disabled={!canAdd} onClick={add}>
            <PlusIcon />
            {t("platform_ai.edit.add")}
          </Button>
        </div>
      )}
      <p role="status" className="sr-only">
        {announcement}
      </p>
    </div>
  );
}
