import { describe, expect, it } from "vitest";
import { AutoSizeDecision } from "../src/auto-size";

describe("initial remote sizing", () => {
  it("waits for explicit session-ready dynamic mode and only decides once", () => {
    const d = new AutoSizeDecision();
    expect(d.decide({ width: 1000, height: 700 }, 1, { width: 1280, height: 800 })).toBeNull();
    d.sessionReady("dynamic");
    expect(d.decide({ width: 1000, height: 700 }, 1, { width: 1280, height: 800 })).toBeNull();
    d.display();
    expect(d.decide({ width: 1000, height: 700 }, 1, { width: 1280, height: 800 })).toEqual({ width: 1000, height: 700 });
    expect(d.decide({ width: 1200, height: 800 }, 2, { width: 1000, height: 700 })).toBeNull();
  });
  it("never sizes fixed sessions or after manual apply", () => {
    const fixed = new AutoSizeDecision();
    fixed.sessionReady("fixed"); fixed.display();
    expect(fixed.decide({ width: 1000, height: 700 }, 1, { width: 1280, height: 800 })).toBeNull();
    const manual = new AutoSizeDecision();
    manual.userApplied(); manual.sessionReady("dynamic"); manual.display();
    expect(manual.decide({ width: 1000, height: 700 }, 1, { width: 1280, height: 800 })).toBeNull();
  });
});
