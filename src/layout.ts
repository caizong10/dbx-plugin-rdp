export interface Size { width: number; height: number }
export interface Rect extends Size { left: number; top: number }

/** Fit the entire remote desktop, preserving its aspect ratio. */
export function fitLayout(view: Size, remote: Size, requestedScale: number): (Size & { limited: boolean }) | null {
  if (![view.width, view.height, remote.width, remote.height].every((n) => Number.isFinite(n) && n > 0)) return null;
  const requested = Number.isFinite(requestedScale) && requestedScale > 0 ? requestedScale : 1;
  const factor = Math.min(view.width / remote.width, view.height / remote.height) * Math.min(1, requested);
  return { width: Math.min(view.width, remote.width * factor), height: Math.min(view.height, remote.height * factor), limited: requested > 1 };
}

// Bound framebuffer allocation even on large, high-DPI displays.
export const MAX_FRAME_PIXELS = 3840 * 2160;

/** Requested framebuffer size in physical pixels, respecting RDP and allocation bounds. */
export function fitResolution(view: Size, devicePixelRatio: number): Size {
  const dpr = Number.isFinite(devicePixelRatio) && devicePixelRatio > 0 ? devicePixelRatio : 1;
  const dimension = (value: number, min: number) => Math.min(8192, Math.max(min, Math.round((Number.isFinite(value) ? Math.max(0, value) : 0) * dpr / 4) * 4));
  let width = dimension(view.width, 256);
  let height = dimension(view.height, 200);
  if (width * height > MAX_FRAME_PIXELS) {
    const factor = Math.sqrt(MAX_FRAME_PIXELS / (width * height));
    width = Math.max(256, Math.floor(width * factor / 4) * 4);
    height = Math.max(200, Math.floor(height * factor / 4) * 4);
    // Minimum dimensions can dominate extremely narrow views.
    if (width * height > MAX_FRAME_PIXELS) {
      width = Math.floor(MAX_FRAME_PIXELS / height / 4) * 4;
    }
  }
  return { width, height };
}

/** Map the actual rendered canvas rectangle to framebuffer pixels. */
export function remotePoint(clientX: number, clientY: number, rect: Rect, remote: Size): { x: number; y: number } | null {
  if (![rect.width, rect.height, remote.width, remote.height].every((n) => Number.isFinite(n) && n > 0) || ![clientX, clientY, rect.left, rect.top].every(Number.isFinite)) return null;
  return {
    x: Math.min(remote.width - 1, Math.max(0, Math.floor((clientX - rect.left) * remote.width / rect.width))),
    y: Math.min(remote.height - 1, Math.max(0, Math.floor((clientY - rect.top) * remote.height / rect.height))),
  };
}
