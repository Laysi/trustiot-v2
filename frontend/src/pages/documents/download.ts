/**
 * Starts the download of `url`, the short-lived address of an original PDF, the way a click on a
 * link would: the browser, not the page, fetches it, so the page's `connect-src` is not involved.
 */
export function startDownload(url: string, fileName: string): void {
  const link = document.createElement("a");
  link.href = url;
  link.download = fileName;
  link.rel = "noopener";
  document.body.append(link);
  link.click();
  link.remove();
}
