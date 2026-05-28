import { getModel, type ModelId } from "@/modules/ai/config";
import { hasAnyKey, type ProviderKeys } from "@/modules/ai/lib/keyring";

export function selectedModelRunsWithoutAppConfiguration(
  modelId: ModelId,
): boolean {
  return getModel(modelId).provider === "codex";
}

export function hasUsableChatProvider(
  keys: ProviderKeys,
  hasLocalModel: boolean,
  selectedModelId: ModelId,
): boolean {
  return (
    hasAnyKey(keys) ||
    hasLocalModel ||
    selectedModelRunsWithoutAppConfiguration(selectedModelId)
  );
}
