// Preloaded by bun test (bunfig.toml): a DOM for the component tests. Tests that touch the network
// or time pass in their own fetch and clock instead of relying on these globals.
import { GlobalRegistrator } from "@happy-dom/global-registrator";

GlobalRegistrator.register({ url: "https://app.example.test/" });
