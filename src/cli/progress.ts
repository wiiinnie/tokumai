// ---------------------------------------------------------------------------
// progress.ts — an honest activity indicator.
//
// The rule this file exists to enforce: THE GLYPH ONLY MOVES WHEN SOMETHING
// REALLY HAPPENED.
//
// A spinner on a timer is a lie. It rotates at the same rate whether frames are
// pouring in, the gateway is wedged, or the process died — so it tells you
// nothing except that a setInterval is still scheduled. Over a mixnet, where a
// request legitimately takes half a minute, that is exactly when you most need
// to know the difference between "working" and "hung".
//
// So there are two independent signals here, and each is separately true:
//
//   the glyph     advances on real events only — a phase change, an arriving
//                 frame. If it sits still, nothing is arriving. That is
//                 information, not a bug.
//   the numbers   elapsed seconds, frame count, bytes received. These tick on a
//                 timer because time really does pass, and they are measured,
//                 not decorative.
//
// When the glyph has been still for a while we say so outright ("idle 12s")
// rather than letting a frozen character read as a crash.
// ---------------------------------------------------------------------------

const GLYPHS = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const IDLE_AFTER_MS = 4000;

export class Progress {
  private glyph = 0;
  private label = "";
  private detail = "";
  private started = Date.now();
  private lastEvent = Date.now();
  private timer: NodeJS.Timeout | null = null;
  private readonly tty: boolean;
  private stopped = false;

  /**
   * @param idleAfterMs how long silence must last before it is called out.
   *   Raise it when gaps are expected by design — during a payment wait the
   *   client polls every 15s, so a 4s threshold would flag "idle" almost
   *   continuously and turn a true signal into noise.
   */
  constructor(initial = "starting", private readonly idleAfterMs = IDLE_AFTER_MS) {
    this.tty = Boolean(process.stderr.isTTY);
    this.label = initial;
    // The 1s tick only refreshes elapsed/idle — it never advances the glyph.
    if (this.tty) this.timer = setInterval(() => this.render(), 1000).unref();
    this.render();
  }

  /** A real state change. Advances the glyph and replaces the label. */
  phase(label: string): void {
    this.label = label;
    this.bump();
  }

  /** Real data arrived. Advances the glyph; detail is measured, not guessed. */
  event(detail?: string): void {
    if (detail !== undefined) this.detail = detail;
    this.bump();
  }

  private bump(): void {
    if (this.stopped) return;
    this.glyph = (this.glyph + 1) % GLYPHS.length;
    this.lastEvent = Date.now();
    this.render();
  }

  private render(): void {
    if (this.stopped) return;
    const secs = Math.round((Date.now() - this.started) / 1000);
    const idleFor = Date.now() - this.lastEvent;

    if (!this.tty) return; // piped output stays clean — no control codes

    const parts = [this.label];
    if (this.detail) parts.push(this.detail);
    parts.push(`${secs}s`);
    // Explain a motionless glyph rather than letting it look like a crash.
    if (idleFor > this.idleAfterMs) parts.push(`idle ${Math.round(idleFor / 1000)}s`);

    const line = `${GLYPHS[this.glyph]} ${parts.join(" · ")}`;
    process.stderr.write(`\r\x1b[2K${line}`);
  }

  /** Clear the line and optionally leave one final, true statement behind. */
  stop(final?: string): void {
    if (this.stopped) return;
    this.stopped = true;
    if (this.timer) clearInterval(this.timer);
    if (this.tty) process.stderr.write("\r\x1b[2K");
    if (final) process.stderr.write(`${final}\n`);
  }

  /** Seconds since construction — used for throughput reporting. */
  get elapsedMs(): number {
    return Date.now() - this.started;
  }
}

/** "1.2 MB" / "512 KB" / "840 B" */
export function humanBytes(n: number): string {
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${Math.round(n / 1024)} KB`;
  return `${n} B`;
}
