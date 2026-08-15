// ---------------------------------------------------------------------------
// format.ts — terminal-side display helpers.
//
// The CLI twin of public/billing.js, with the same two rules: never compute a
// price from tokens (the server sends a finished one), and never display
// costScrai — that number rides in the frame for our accounting and stays out
// of the interface.
// ---------------------------------------------------------------------------

import type { TokenUsage } from "../types.js";

const fmt = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });

export function formatScraiCli(scrai: number): string {
  return fmt.format(Math.max(0, Math.round(scrai || 0)));
}

/** "612 in · 208 out · 96 thinking · 400 cached" */
export function formatTokensCli(usage: TokenUsage | undefined): string {
  if (!usage) return "";
  const input = (usage.inputTokens || 0) + (usage.cachedInputTokens || 0) + (usage.audioInputTokens || 0);
  const parts = [`${fmt.format(input)} in`, `${fmt.format(usage.outputTokens || 0)} out`];
  if (usage.thoughtTokens) parts.push(`${fmt.format(usage.thoughtTokens)} thinking`);
  if (usage.cachedInputTokens) parts.push(`${fmt.format(usage.cachedInputTokens)} cached`);
  return parts.join(" · ");
}
