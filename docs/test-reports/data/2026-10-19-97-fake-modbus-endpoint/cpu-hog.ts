// Busy-loops on N workers until killed, to load every core.
const n = Number(process.argv[2] ?? 16);
const code = "for (;;) {}";
for (let i = 0; i < n; i++) new Worker(URL.createObjectURL(new Blob([code])));
await new Promise(() => {});
