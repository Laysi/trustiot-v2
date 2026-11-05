/**
 * Captures the files a page offers for download (`downloadJson`): happy-dom has no object URLs and
 * a click on a link must not navigate. `restore()` puts the browser's functions back.
 */
export function captureDownloads(): { files: { name: string; text: Promise<string> }[]; restore(): void } {
  const files: { name: string; text: Promise<string> }[] = [];
  const create = URL.createObjectURL;
  const revoke = URL.revokeObjectURL;
  const click = HTMLAnchorElement.prototype.click;
  const blobs = new Map<string, Blob>();
  URL.createObjectURL = (blob: Blob | MediaSource) => {
    const url = `blob:test/${blobs.size}`;
    blobs.set(url, blob as Blob);
    return url;
  };
  URL.revokeObjectURL = () => {};
  HTMLAnchorElement.prototype.click = function capture(this: HTMLAnchorElement) {
    const blob = blobs.get(this.href);
    if (blob && this.download) files.push({ name: this.download, text: blob.text() });
  };
  return {
    files,
    restore: () => {
      URL.createObjectURL = create;
      URL.revokeObjectURL = revoke;
      HTMLAnchorElement.prototype.click = click;
    },
  };
}
