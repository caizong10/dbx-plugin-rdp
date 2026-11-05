import { describe, expect, it } from "vitest";
import { fitLayout, fitResolution, MAX_FRAME_PIXELS, remotePoint } from "../src/layout";

describe("remote layout", () => {
  it("fits the complete desktop and preserves aspect ratio", () => {
    expect(fitLayout({ width: 1000, height: 700 }, { width: 1920, height: 1080 }, 2)).toEqual({
      width: 1000,
      height: 562.5,
      limited: true,
    });
  });
  it("does not let an enlarged request introduce scrollbars", () => {
    const result = fitLayout({ width: 1000, height: 700 }, { width: 800, height: 600 }, 1.5)!;
    expect(result.width).toBeLessThanOrEqual(1000);
    expect(result.height).toBeLessThanOrEqual(700);
  });
  it("uses device pixels in multiples accepted by the remote protocol", () => {
    expect(fitResolution({ width: 1000, height: 700 }, 1.5)).toEqual({ width: 1500, height: 1052 });
  });
  it("caps total pixels and preserves four-pixel alignment at high DPR", () => {
    for (const view of [{ width: 7680, height: 4320 }, { width: 100, height: 9000 }]) {
      const size = fitResolution(view, 2);
      expect(size.width * size.height).toBeLessThanOrEqual(MAX_FRAME_PIXELS);
      expect(size.width % 4).toBe(0);
      expect(size.height % 4).toBe(0);
    }
    expect(fitResolution({ width: 1000, height: 700 }, 2)).toEqual({ width: 2000, height: 1400 });
  });
  it("maps pointer coordinates using the rendered rectangle", () => {
    expect(remotePoint(150, 125, { left: 100, top: 50, width: 500, height: 250 }, { width: 1000, height: 500 })).toEqual({ x: 100, y: 150 });
    expect(remotePoint(0, 0, { left: 100, top: 50, width: 500, height: 250 }, { width: 1000, height: 500 })).toEqual({ x: 0, y: 0 });
  });
});
