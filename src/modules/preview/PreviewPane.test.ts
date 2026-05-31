import { describe, expect, it } from "vitest";
import {
  isLocalPreviewUrl,
  previewAllowForUrl,
  previewSandboxForUrl,
} from "./PreviewPane";

describe("PreviewPane iframe sandbox", () => {
  it("keeps full dev-preview capability for loopback URLs", () => {
    expect(isLocalPreviewUrl("http://localhost:5173")).toBe(true);
    expect(isLocalPreviewUrl("http://127.0.0.1:3000")).toBe(true);
    expect(isLocalPreviewUrl("http://[::1]:5173")).toBe(true);
    expect(previewSandboxForUrl("http://localhost:5173")).toContain(
      "allow-same-origin",
    );
    expect(previewSandboxForUrl("http://localhost:5173")).toContain(
      "allow-downloads",
    );
    expect(previewAllowForUrl("http://localhost:5173")).toContain(
      "clipboard-write",
    );
  });

  it("does not grant local-only privileges to external URLs", () => {
    expect(isLocalPreviewUrl("https://example.com")).toBe(false);
    expect(previewSandboxForUrl("https://example.com")).not.toContain(
      "allow-same-origin",
    );
    expect(previewSandboxForUrl("https://example.com")).not.toContain(
      "allow-downloads",
    );
    expect(previewAllowForUrl("https://example.com")).toBe("fullscreen");
  });

  it("never allows iframe top navigation", () => {
    for (const url of ["http://localhost:5173", "https://example.com"]) {
      expect(previewSandboxForUrl(url)).not.toContain("allow-top-navigation");
    }
  });

  it("keeps popups escaped from the sandbox when popups are allowed", () => {
    for (const url of ["http://localhost:5173", "https://example.com"]) {
      const sandbox = previewSandboxForUrl(url);
      expect(sandbox).toContain("allow-popups");
      expect(sandbox).toContain("allow-popups-to-escape-sandbox");
    }
  });
});
