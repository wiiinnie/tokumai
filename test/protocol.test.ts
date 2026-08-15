/**
 * protocol.test.ts — the wire envelope.
 *
 * This is the one contract two independently evolving programs share (and, once
 * the Rust core lands, two languages). Parsing has to be total: every malformed
 * input becomes an error response, never a thrown exception that would take the
 * server's message loop down with it.
 */

import assert from "node:assert/strict";
import { PROTOCOL_VERSION, parseRequest, parseResponse, errorResponse, newId } from "../src/protocol.js";

/* 1) A well-formed chat request round-trips with its fields intact */
const chat = parseRequest(
  JSON.stringify({
    v: PROTOCOL_VERSION,
    kind: "chat",
    id: "abc",
    model: "gemini-3.5-flash-lite",
    messages: [{ role: "user", content: "hi" }],
    maxTokens: 512,
  }),
);
assert.ok(chat.ok);
assert.equal(chat.req.kind, "chat");
if (chat.req.kind === "chat") {
  assert.equal(chat.req.model, "gemini-3.5-flash-lite");
  assert.equal(chat.req.maxTokens, 512);
  assert.equal(chat.req.messages.length, 1);
}

/* 2) models needs nothing but a version and an id */
const models = parseRequest(JSON.stringify({ v: PROTOCOL_VERSION, kind: "models", id: "x" }));
assert.ok(models.ok);
assert.equal(models.req.kind, "models");

/* 3) Every malformed shape is an error VALUE, never a throw — and the id is
      preserved where possible so the client can match the failure to its request */
for (const bad of ["", "{", "null", "[]", '"a string"']) {
  const r = parseRequest(bad);
  assert.equal(r.ok, false, `expected failure for ${JSON.stringify(bad)}`);
}

const wrongVersion = parseRequest(JSON.stringify({ v: 99, kind: "chat", id: "keepme" }));
assert.equal(wrongVersion.ok, false);
if (!wrongVersion.ok) {
  assert.equal(wrongVersion.id, "keepme");
  assert.match(wrongVersion.error, /unsupported protocol version 99/);
}

const unknownKind = parseRequest(JSON.stringify({ v: PROTOCOL_VERSION, kind: "launch-missiles", id: "k" }));
assert.equal(unknownKind.ok, false);
if (!unknownKind.ok) assert.match(unknownKind.error, /unknown kind/);

/* 4) A chat missing its required fields is rejected, not half-accepted */
const noModel = parseRequest(JSON.stringify({ v: PROTOCOL_VERSION, kind: "chat", id: "n", messages: [] }));
assert.equal(noModel.ok, false);
const noMessages = parseRequest(JSON.stringify({ v: PROTOCOL_VERSION, kind: "chat", id: "n", model: "m" }));
assert.equal(noMessages.ok, false);

/* 5) Responses parse back, and garbage yields null rather than throwing */
const ok = parseResponse(JSON.stringify(errorResponse("id1", "boom")));
assert.ok(ok);
assert.equal(ok.kind, "error");
if (ok.kind === "error") assert.equal(ok.error, "boom");
assert.equal(parseResponse("not json"), null);
assert.equal(parseResponse("42"), null);

/* 6) Ids are unique — they are what lets more than one request share a client */
assert.notEqual(newId(), newId());

console.log("all protocol checks passed");
