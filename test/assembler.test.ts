/**
 * assembler.test.ts — out-of-order reassembly.
 *
 * This is the one piece of the streaming path that fails silently when it is
 * wrong: a scrambled or truncated answer reads as the model's own words, not as
 * a transport bug. So the cases below are deliberately hostile — reordering,
 * duplicates, gaps that fill late, and gaps that never fill.
 */

import assert from "node:assert/strict";
import { StreamAssembler } from "../src/assembler.js";

/* 1) In-order delivery emits as it goes */
{
  const a = new StreamAssembler();
  assert.equal(a.chunk(0, "Hello "), "Hello ");
  assert.equal(a.chunk(1, "world"), "world");
  a.end(2);
  assert.ok(a.done);
  assert.equal(a.state.emitted, "Hello world");
  assert.equal(a.truncated, false);
}

/* 2) Out-of-order: an early arrival emits nothing until its gap is filled,
      then the whole run flushes at once */
{
  const a = new StreamAssembler();
  assert.equal(a.chunk(2, "three "), "");
  assert.equal(a.chunk(1, "two "), "");
  assert.equal(a.chunk(0, "one "), "one two three ");
  a.end(3);
  assert.ok(a.done);
  assert.equal(a.state.emitted, "one two three ");
}

/* 3) Fully reversed delivery still reassembles correctly */
{
  const a = new StreamAssembler();
  const parts = ["a", "b", "c", "d", "e"];
  for (let i = parts.length - 1; i >= 0; i--) a.chunk(i, parts[i]!);
  a.end(parts.length);
  assert.equal(a.state.emitted, "abcde");
  assert.ok(a.done);
}

/* 4) Duplicates are ignored, not re-emitted — a replayed frame must not
      double a word in the answer */
{
  const a = new StreamAssembler();
  a.chunk(0, "x");
  assert.equal(a.chunk(0, "x"), "");
  a.chunk(1, "y");
  assert.equal(a.chunk(1, "y"), "");
  a.end(2);
  assert.equal(a.state.emitted, "xy");
}

/* 5) A gap that never fills is reported as truncation, NOT quietly accepted.
      This is the case that would otherwise put words in the model's mouth. */
{
  const a = new StreamAssembler();
  a.chunk(0, "start ");
  a.chunk(2, "end");     // 1 never arrives
  a.end(3);
  assert.equal(a.done, false);
  assert.equal(a.truncated, true);
  assert.deepEqual(a.state.missing, [1]);
  assert.equal(a.state.emitted, "start "); // never emits past the hole
}

/* 6) Before the terminator, an incomplete stream is NOT truncated — it is
      merely unfinished. Confusing the two would abort healthy streams. */
{
  const a = new StreamAssembler();
  a.chunk(0, "partial");
  assert.equal(a.truncated, false);
  assert.equal(a.done, false);
}

/* 7) Empty answer: end with zero chunks is immediately complete */
{
  const a = new StreamAssembler();
  a.end(0);
  assert.ok(a.done);
  assert.equal(a.state.emitted, "");
}

/* 8) A late chunk arriving after end still completes the stream */
{
  const a = new StreamAssembler();
  a.chunk(0, "one ");
  a.end(2);
  assert.equal(a.truncated, true);
  assert.equal(a.chunk(1, "two"), "two");
  assert.ok(a.done);
  assert.equal(a.truncated, false);
  assert.equal(a.state.emitted, "one two");
}

console.log("all assembler checks passed");
