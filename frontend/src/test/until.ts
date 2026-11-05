/** How long a state change that goes through the server and a re-read may take on a loaded machine. */
export const LONG = 20_000;

/**
 * Waits for `done` with a generous bound that names what was awaited. Used where RTL's `waitFor`
 * was seen to hold the event loop while a held request was waiting to be released.
 */
export async function until(done: () => boolean, what: string): Promise<void> {
  const deadline = Date.now() + LONG;
  while (!done()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resume) => setTimeout(resume, 25));
  }
}
