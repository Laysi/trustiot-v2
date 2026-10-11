// Busy-loop load generator: bun burn.ts <workers> <seconds>
import { isMainThread } from "node:worker_threads";
const workers = Number(process.argv[2] ?? 8);
const seconds = Number(process.argv[3] ?? 600);
if (isMainThread) {
  for (let i = 0; i < workers; i++) new Worker(import.meta.url);
  setTimeout(() => process.exit(0), seconds * 1000);
} else {
  const end = Date.now() + seconds * 1000;
  let x = 0;
  while (Date.now() < end) x = (x * 31 + 7) % 1000003;
}
