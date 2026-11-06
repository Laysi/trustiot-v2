/** The query key of the model roles (the platform's, not a tenant's). */
export const MODEL_ROLES_KEY = ["platform", "ai", "model-roles"] as const;

export const promptsKey = ["platform", "ai", "prompts"] as const;
export const promptKey = (service: string, role: string) => ["platform", "ai", "prompt", service, role] as const;
export const versionsKey = (service: string, role: string) => ["platform", "ai", "versions", service, role] as const;
export const versionKey = (service: string, role: string, version: number) =>
  ["platform", "ai", "version", service, role, version] as const;
export const evalOptionsKey = (service: string, role: string) =>
  ["platform", "ai", "eval-options", service, role] as const;
export const evalRunKey = (service: string, role: string, id: string) =>
  ["platform", "ai", "eval", service, role, id] as const;
export const currentEvalKey = (service: string, role: string) =>
  ["platform", "ai", "eval-current", service, role] as const;
export const usageMonthsKey = ["platform", "ai", "usage-months"] as const;
export const usageKey = (month: string) => ["platform", "ai", "usage", month] as const;
export const reconciliationKey = (month: string) => ["platform", "ai", "reconciliation", month] as const;
