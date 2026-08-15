// ---------------------------------------------------------------------------
// assembler.ts — put a streamed answer back in order.
//
// Mixnet frames are independent messages. Each one picks its own route through
// the mix nodes, so frame 7 can overtake frame 5 and a chunk can simply never
// show up. TCP hides all of this; we do not have TCP.
//
// So the client buffers what arrives early and emits only what it can emit in
// order. Two states matter and must not be confused:
//
//   incomplete + end not seen   -> still streaming, keep waiting
//   incomplete + end seen       -> a chunk is genuinely lost, fail loudly
//
// The second case is why `chat.end` carries a total. Without it a lost chunk is
// indistinguishable from a slow one, and the honest failure ("chunk 4 of 9
// never arrived") degrades into a silent truncation the user reads as the
// model's own words. That is the bug this file exists to prevent.
//
// Pure and synchronous on purpose — no sockets, no timers, so the ordering
// logic is testable without a mixnet.
// ---------------------------------------------------------------------------

export interface AssemblerState {
  /** Contiguous text emitted so far, in order. */
  emitted: string;
  /** True once every chunk up to the announced total has been emitted. */
  complete: boolean;
  /** Sequence numbers still missing, once the total is known. */
  missing: number[];
}

export class StreamAssembler {
  private pending = new Map<number, string>();
  private next = 0;
  private total: number | null = null;
  private text = "";

  /**
   * Take one chunk. Returns the text that became emittable as a result — which
   * is "" when the chunk arrived early, and several chunks' worth when it
   * filled a gap.
   */
  chunk(seq: number, delta: string): string {
    // Ignore replays and anything at or before what we already emitted.
    if (seq < this.next || this.pending.has(seq)) return "";
    this.pending.set(seq, delta);

    let out = "";
    while (this.pending.has(this.next)) {
      out += this.pending.get(this.next)!;
      this.pending.delete(this.next);
      this.next += 1;
    }
    this.text += out;
    return out;
  }

  /** Record the announced chunk count from `chat.end`. */
  end(chunks: number): void {
    this.total = chunks;
  }

  get state(): AssemblerState {
    const missing: number[] = [];
    if (this.total !== null) {
      for (let i = this.next; i < this.total; i++) {
        if (!this.pending.has(i)) missing.push(i);
      }
    }
    return {
      emitted: this.text,
      complete: this.total !== null && this.next >= this.total,
      missing,
    };
  }

  /** True when the terminator arrived and nothing is outstanding. */
  get done(): boolean {
    return this.state.complete;
  }

  /** True when the terminator arrived but chunks are missing — a real failure. */
  get truncated(): boolean {
    return this.total !== null && this.next < this.total;
  }
}
