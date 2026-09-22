// Type declarations for prune.js (a plain ES module shared by the UI and tests).
export interface PruneOpts {
  topicShift: boolean;
  whitespace: boolean;
  requestTrim: boolean;
  handover: boolean;
  /** Guaranteed memory floor: the last N complete Q&A pairs are always sent. */
  ctxPairs?: number;
}
export interface PruneMsg {
  role: string;
  content: string;
  [k: string]: unknown;
}
export const PRUNE_DEFAULTS: PruneOpts;
export function terms(text: string): Set<string>;
export function jaccard(a: Set<string>, b: Set<string>): number;
export function isFollowup(text: string): boolean;
export function normalizeWhitespace(s: string): string;
export function trimRequest(s: string): string;
export function pruneMessages(
  messages: PruneMsg[],
  opts?: PruneOpts,
  cfg?: { keepRecentPairs?: number; minRecentPairs?: number; threshold?: number },
): PruneMsg[];
export function pruneForHandover(messages: PruneMsg[], opts?: PruneOpts): PruneMsg[];
