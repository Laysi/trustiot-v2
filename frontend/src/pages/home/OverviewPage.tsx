import { useSearchParams } from "react-router";

import { GenericHome } from "./GenericHome.tsx";

/**
 * The generic home as its own page, for the UI preview (`preview.ts`): `?ai=paused` shows it as
 * the home falls back to it when the assistant could not be reached. Where the home chooses
 * between the conversation and this layout at load time, the choice calls `GenericHome` itself.
 */
export function OverviewPage() {
  const [params] = useSearchParams();
  return <GenericHome aiPaused={params.get("ai") === "paused"} />;
}
