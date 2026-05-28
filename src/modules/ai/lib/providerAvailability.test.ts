import { describe, expect, it } from "vitest";

import { EMPTY_PROVIDER_KEYS } from "@/modules/ai/lib/keyring";
import { hasUsableChatProvider } from "./providerAvailability";

describe("hasUsableChatProvider", () => {
  it("allows Codex CLI without an API key or local model settings", () => {
    expect(hasUsableChatProvider(EMPTY_PROVIDER_KEYS, false, "codex-cli")).toBe(
      true,
    );
  });

  it("still requires a key for cloud defaults", () => {
    expect(hasUsableChatProvider(EMPTY_PROVIDER_KEYS, false, "gpt-5.5")).toBe(
      false,
    );
  });

  it("allows configured local providers", () => {
    expect(hasUsableChatProvider(EMPTY_PROVIDER_KEYS, true, "gpt-5.5")).toBe(
      true,
    );
  });
});
