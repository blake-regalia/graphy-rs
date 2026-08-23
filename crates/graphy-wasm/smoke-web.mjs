// Smoke the web-target package through the same GraphyStoreServer boundary
// used by browser workers. Build pkg-web with the command in docs/11-wasm.md.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import init, { GraphyStoreServer } from "./pkg-web/graphy_wasm.js";

const wasm = await readFile(new URL("./pkg-web/graphy_wasm_bg.wasm", import.meta.url));
await init({ module_or_path: wasm });

const server = new GraphyStoreServer();
const body = new TextEncoder().encode("<urn:s> <urn:p> <urn:o> .");
let reply = await server.handleRequest(
  "PUT",
  "/graphs",
  JSON.stringify({ "content-type": "text/turtle" }),
  body,
);
assert.equal(reply.status, 204);

reply = await server.handleRequest("GET", "/sparql?query=ASK%7B%3Fs%20%3Fp%20%3Fo%7D", "{}");
assert.equal(reply.status, 200);
assert.equal(JSON.parse(new TextDecoder().decode(reply.body)).boolean, true);
console.log("graphy-wasm web smoke: PASS");
