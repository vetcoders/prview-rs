import { describe, expect, it } from "vitest";

import { add } from "./math.js";

describe("bounded runtime fixture", () => {
  it("keeps one real Vitest worker observable", async () => {
    await new Promise((resolve) => setTimeout(resolve, 750));
    expect(add(2, 3)).toBe(5);
  });
});
