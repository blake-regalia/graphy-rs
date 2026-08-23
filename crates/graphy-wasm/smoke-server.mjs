// StoreServer node smoke: exercises the wasm-bindgen boundary of the
// in-process protocol router (build the nodejs package first:
// `wasm-pack build --target nodejs --out-dir pkg .`). Prints VERDICT: PASS.
import assert from "node:assert/strict";
import { GraphyStoreServer } from "./pkg/graphy_wasm.js";

const text = (reply) => Buffer.from(reply.body).toString("utf8");

// boot fresh, seed via dataset-level GSP PUT
const server = new GraphyStoreServer(undefined, undefined);
let reply = await server.handleRequest(
  "PUT",
  "/graphs",
  JSON.stringify({ "content-type": "application/trig" }),
  new TextEncoder().encode("@prefix ex: <http://e/> . ex:s ex:p 1, 2 ."),
);
assert.equal(reply.status, 204, text(reply));

// SPARQL Protocol GET → SRJ + ETag
reply = await server.handleRequest("GET", "/sparql?query=ASK%7B%7D", "{}", undefined);
assert.equal(reply.status, 200, text(reply));
assert.ok(reply.headers.etag, "etag header");
assert.equal(JSON.parse(text(reply)).boolean, true);

// update through the router, NOW() must not panic (injected clock)
reply = await server.handleRequest(
  "POST",
  "/sparql",
  JSON.stringify({ "content-type": "application/sparql-update" }),
  new TextEncoder().encode(
    "PREFIX ex: <http://e/> INSERT { ex:s ex:at ?t } WHERE { BIND(NOW() AS ?t) }",
  ),
);
assert.equal(reply.status, 204, text(reply));
assert.equal(server.size(), 3);

// persistence: drain → restore in a second instance
const log = server.drainLog();
assert.ok(log.length > 0, "captured WAL frames");
const restored = new GraphyStoreServer(log, undefined);
assert.equal(restored.size(), 3);

// strict restore rejects foreign bytes; lenient truncates them away
assert.throws(() => new GraphyStoreServer(new TextEncoder().encode("junk"), undefined, true));
assert.equal(new GraphyStoreServer(new TextEncoder().encode("junk"), undefined, undefined).size(), 0);
assert.equal(new GraphyStoreServer(restored.packLog(), undefined, true).size(), 3);

// read-only boot rejects writes
const frozen = new GraphyStoreServer(restored.packLog(), true);
reply = await frozen.handleRequest(
  "POST",
  "/sparql",
  JSON.stringify({ "content-type": "application/sparql-update" }),
  new TextEncoder().encode("INSERT DATA { <http://e/s> <http://e/p> 9 }"),
);
assert.equal(reply.status, 403, text(reply));
reply = await frozen.handleRequest("GET", "/sparql?query=ASK%7B%7D", "{}", undefined);
assert.equal(JSON.parse(text(reply)).boolean, true);

console.log("VERDICT: PASS");
