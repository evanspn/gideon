// Function-level tests for the OAuth helper: verifier bounds, response
// minimization, and the per-instance rate limit. Run: npm run test:api
import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import handler from "../../api/mal-oauth.js";

process.env.MAL_CLIENT_ID = "cid-test";
process.env.MAL_CLIENT_SECRET = "secret-test";

let upstreamBodies;
beforeEach(() => {
  upstreamBodies = [];
  globalThis.fetch = async (url, init) => {
    upstreamBodies.push(init.body.toString());
    return new Response(
      JSON.stringify({ access_token: "AT", refresh_token: "RT", expires_in: 1, extra: "never-forward" }),
      { status: 200 }
    );
  };
});

let ipCounter = 0;
function run(req) {
  const res = {
    headers: {},
    statusCode: 200,
    body: null,
    setHeader(k, v) {
      this.headers[k.toLowerCase()] = v;
    },
    status(c) {
      this.statusCode = c;
      return this;
    },
    json(b) {
      this.body = b;
      return this;
    },
  };
  return handler(
    {
      method: "POST",
      headers: { "x-forwarded-for": req.ip || `10.0.0.${++ipCounter}` },
      query: {},
      body: undefined,
      ...req,
    },
    res
  ).then(() => res);
}

test("config exposes the client id and nothing else", async () => {
  const res = await run({ method: "GET", query: { action: "config" } });
  assert.deepEqual(Object.keys(res.body).sort(), ["client_id", "redirect_uri"]);
});

test("verifier outside RFC 7636 bounds is refused", async () => {
  const res = await run({ query: { action: "token" }, body: { code: "c", verifier: "short" } });
  assert.equal(res.statusCode, 400);
  assert.equal(upstreamBodies.length, 0);
});

test("a valid exchange forwards the secret upstream but never to the browser", async () => {
  const res = await run({
    query: { action: "token" },
    body: { code: "c", verifier: "v".repeat(60) },
  });
  assert.equal(res.statusCode, 200);
  assert.match(upstreamBodies[0], /client_secret=secret-test/);
  assert.deepEqual(Object.keys(res.body).sort(), ["access_token", "expires_in", "refresh_token"]);
});

test("refresh requires a refresh_token", async () => {
  const res = await run({ query: { action: "refresh" }, body: {} });
  assert.equal(res.statusCode, 400);
});

test("the same source is rate limited after a burst", async () => {
  let last;
  for (let i = 0; i < 11; i++) {
    last = await run({
      ip: "10.9.9.9",
      query: { action: "refresh" },
      body: { refresh_token: "r" },
    });
  }
  assert.equal(last.statusCode, 429);
});
