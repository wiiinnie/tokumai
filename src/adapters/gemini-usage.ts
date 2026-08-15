// ---------------------------------------------------------------------------
// gemini-usage.ts — the one place that understands Gemini's usageMetadata.
//
// Two things the naive reading gets wrong, both of them money:
//
//   1. candidatesTokenCount does NOT include thinking. Google bills output as
//      candidates + thoughts, so that sum is outputTokens. Reading candidates
//      alone undercharges every thinking model by a lot.
//   2. promptTokenCount INCLUDES cached tokens, which bill at ~1/10th the rate.
//      Uncached input is prompt - cached.
//
// Streaming repeats usageMetadata cumulatively across frames, so the last frame
// with a total is authoritative — merge, never sum.
// ---------------------------------------------------------------------------

import { EMPTY_USAGE } from "../billing.js";
import type { TokenUsage } from "../types.js";

/** The field as it appears on Gemini's REST/SSE responses. */
export interface GeminiUsageMetadata {
  promptTokenCount?: number;
  candidatesTokenCount?: number;
  thoughtsTokenCount?: number;
  cachedContentTokenCount?: number;
  toolUsePromptTokenCount?: number;
  totalTokenCount?: number;
  promptTokensDetails?: Array<{ modality?: string; tokenCount?: number }>;
  cacheTokensDetails?: Array<{ modality?: string; tokenCount?: number }>;
}

const n = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) && v > 0 ? v : 0);

function audioTokens(details: GeminiUsageMetadata["promptTokensDetails"]): number {
  if (!Array.isArray(details)) return 0;
  return details
    .filter((d) => (d?.modality ?? "").toUpperCase() === "AUDIO")
    .reduce((sum, d) => sum + n(d.tokenCount), 0);
}

/** Translate one usageMetadata object into the neutral format. */
export function parseGeminiUsage(meta: GeminiUsageMetadata | undefined | null): TokenUsage | null {
  if (!meta) return null;

  const prompt = n(meta.promptTokenCount) + n(meta.toolUsePromptTokenCount);
  const cached = Math.min(n(meta.cachedContentTokenCount), prompt);
  const audio = Math.min(audioTokens(meta.promptTokensDetails), Math.max(prompt - cached, 0));
  const thoughts = n(meta.thoughtsTokenCount);
  const out = n(meta.candidatesTokenCount) + thoughts;

  if (prompt === 0 && out === 0) return null;

  return {
    ...EMPTY_USAGE,
    inputTokens: Math.max(prompt - cached - audio, 0),
    cachedInputTokens: cached,
    audioInputTokens: audio,
    outputTokens: out,
    thoughtTokens: thoughts,
    totalTokens: n(meta.totalTokenCount) || prompt + out,
  };
}

/**
 * Fold what we've seen so far. The later frame replaces the earlier one, except
 * that a partial frame must never shrink a total we already had.
 */
export function mergeGeminiUsage(
  previous: TokenUsage | null,
  meta: GeminiUsageMetadata | undefined,
): TokenUsage | null {
  const next = parseGeminiUsage(meta);
  if (!next) return previous;
  if (!previous) return next;
  return (next.totalTokens ?? 0) >= (previous.totalTokens ?? 0) ? next : previous;
}
