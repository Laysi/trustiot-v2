/**
 * Saves `value` as a JSON file named `filename` through the browser's download: a short-lived
 * object URL on a link that is clicked. The file never goes through the server again.
 */
export function downloadJson(filename: string, value: unknown): void {
  const blob = new Blob([`${JSON.stringify(value, null, 2)}\n`], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.rel = "noopener";
  document.body.append(link);
  link.click();
  link.remove();
  // The download has started when click returns; the URL is released on the next turn.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}
