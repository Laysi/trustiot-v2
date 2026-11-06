/** Where the platform AI pages live (docs/design/platform-admin/design.md, 「資訊架構」: `/platform/...`). */
export const PLATFORM_AI_PATH = "/platform/ai";
export const MODEL_ROLES_PATH = `${PLATFORM_AI_PATH}/model-roles`;
export const PROMPTS_PATH = `${PLATFORM_AI_PATH}/prompts`;
export const USAGE_PATH = `${PLATFORM_AI_PATH}/usage`;

/** A prompt's page: the service and the role it belongs to. */
export function promptPath(service: string, role: string): string {
  return `${PROMPTS_PATH}/${encodeURIComponent(service)}/${encodeURIComponent(role)}`;
}

/** A prompt's versions. */
export function promptVersionsPath(service: string, role: string): string {
  return `${promptPath(service, role)}/versions`;
}
