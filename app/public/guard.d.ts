// Type declarations for guard.js (shared by the UI and tests).
export interface GuardOpts {
  enabled: boolean;
  scanText: boolean;
  scanImages: boolean;
}
export interface Finding {
  type: string;
  label: string;
}
export const GUARD_DEFAULTS: GuardOpts;
export function scanText(text: string, opts?: { ocr?: boolean }): Finding[];
