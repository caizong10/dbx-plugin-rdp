import { fitResolution, type Size } from "./layout";

/** One decision per session, never per frame or framebuffer generation. */
export class AutoSizeDecision {
  private finished = false;
  private manual = false;
  private ready = false;
  private mode: string | undefined;
  private displayed = false;

  sessionReady(mode: string): void { this.ready = true; this.mode = mode; }
  display(): void { this.displayed = true; }
  userApplied(): void { this.manual = true; }
  decide(view: Size, dpr: number, remote: Size): Size | null {
    if (this.finished || this.manual || !this.ready || this.mode !== "dynamic" || !this.displayed ||
        !Number.isFinite(view.width) || !Number.isFinite(view.height) || view.width <= 0 || view.height <= 0) return null;
    this.finished = true;
    const target = fitResolution(view, dpr);
    return target.width === remote.width && target.height === remote.height ? null : target;
  }
}
