import { createElement, type ReactNode } from "react";

import { ApiError } from "../api/errors.ts";
import type { QuestionTemplate, QuestionTemplatesApi } from "../api/pending/question-templates.ts";
import { TemplateSourceProvider } from "../chat/completion/templates.tsx";

/**
 * The question templates (`api/pending/question-templates.ts`) in memory: five templates over the
 * semantic-layer tools, in both languages, each with an example filled in with a fictional device.
 * Every call is recorded (`calls`, the language asked for); `failNext` makes the next call fail,
 * and `hold` keeps the next call waiting until the returned function is called.
 */

const DEVICE = { zh: "空壓機 A2", en: "Compressor A2" };

function templates(locale: "zh-TW" | "en"): QuestionTemplate[] {
  const zh = locale === "zh-TW";
  const group = { energy: zh ? "用電" : "Energy use", faults: zh ? "異常與斷訊" : "Anomalies and lost links" };
  const blank = {
    device: zh ? "［設備］" : "［Device］",
    range: zh ? "［時間範圍］" : "［Time range］",
  };
  const slots = {
    device: { label: zh ? "設備" : "Device", kind: "device" as const },
    range: { label: zh ? "時間範圍" : "Time range", kind: "range" as const },
  };
  const name = zh ? DEVICE.zh : DEVICE.en;
  return [
    {
      id: "energy-of-device",
      group: group.energy,
      text: zh ? `${blank.device}在${blank.range}的用電量` : `Energy used by ${blank.device} in ${blank.range}`,
      slots: [slots.device, slots.range],
      tool: zh ? "用電量工具" : "Energy tool",
      example: zh ? `${name} 上週的用電量` : `Energy used by ${name} last week`,
    },
    {
      id: "energy-compare",
      group: group.energy,
      text: zh
        ? `${blank.device}${blank.range}的用電跟前一段比`
        : `${blank.device}: energy in ${blank.range} against the period before`,
      slots: [slots.device, slots.range],
      tool: zh ? "用電量工具・兩段比較" : "Energy tool, two periods",
      example: zh ? `${name} 本週的用電跟前一段比` : `${name}: energy this week against the period before`,
    },
    {
      id: "energy-top",
      group: group.energy,
      text: zh ? `${blank.range}用電最多的 5 台設備` : `The 5 devices that used the most energy in ${blank.range}`,
      slots: [slots.range],
      tool: zh ? "用電量工具・排名" : "Energy tool, ranking",
      example: zh ? "上週用電最多的 5 台設備" : "The 5 devices that used the most energy last week",
    },
    {
      id: "anomaly-segments",
      group: group.faults,
      text: zh
        ? `${blank.device}在${blank.range}有哪些異常區段`
        : `Anomalous stretches of ${blank.device} in ${blank.range}`,
      slots: [slots.device, slots.range],
      tool: zh ? "異常區段工具" : "Anomaly tool",
      example: zh ? `${name} 昨天有哪些異常區段` : `Anomalous stretches of ${name} yesterday`,
    },
    {
      id: "lost-links",
      group: group.faults,
      text: zh
        ? `${blank.range}有哪些設備斷訊、斷了多久`
        : `Which devices lost their link in ${blank.range}, and for how long`,
      slots: [slots.range],
      tool: zh ? "斷訊工具" : "Lost-link tool",
      example: null,
    },
  ];
}

export class FakeQuestionTemplates {
  /** The language of every call, in order. */
  readonly calls: string[] = [];
  private failure: ApiError | null = null;
  private held: Promise<void> | null = null;

  failNext(error: ApiError = new ApiError(503, { code: "unavailable", message_key: "error.unavailable" })) {
    this.failure = error;
  }

  hold(): () => void {
    let release = () => {};
    this.held = new Promise<void>((resolve) => {
      release = resolve;
    });
    return release;
  }

  api(): QuestionTemplatesApi {
    return {
      listTemplates: async (locale) => {
        this.calls.push(locale);
        const held = this.held;
        this.held = null;
        if (held) await held;
        const failure = this.failure;
        this.failure = null;
        if (failure) throw failure;
        return templates(locale === "zh-TW" ? "zh-TW" : "en");
      },
    };
  }
}

export const preview = {
  name: "Question templates",
  provider: (_seed: "normal" | "empty") => {
    const source = new FakeQuestionTemplates().api();
    return function QuestionTemplatesPreviewProvider({ children }: { children: ReactNode }) {
      return createElement(TemplateSourceProvider, { source, children });
    };
  },
};
