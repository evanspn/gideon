// Function-level tests for the MAL proxy: the allowlists and cache pinning
// are security boundaries, so they get direct coverage here (the browser
// suite mocks this endpoint away). Run: npm run test:api
import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import handler from "./mal.js";

process.env.MAL_CLIENT_ID = "cid-test";

let upstreamCalls;
beforeEach(() => {
  upstreamCalls = [];
  globalThis.fetch = async (url, init) => {
    upstreamCalls.push({ url: String(url), init });
    return new Response('{"data":[]}', { status: 200 });
  };
});

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
    send(b) {
      this.body = b;
      return this;
    },
  };
  return handler({ method: "GET", headers: {}, query: {}, body: undefined, ...req }, res).then(
    () => res
  );
}

test("public catalog GET goes out with the client id and edge caching", async () => {
  const res = await run({ query: { path: "manga/ranking?ranking_type=all&limit=3" } });
  assert.equal(res.statusCode, 200);
  assert.match(res.headers["cache-control"], /s-maxage/);
  assert.equal(upstreamCalls[0].init.headers["X-MAL-CLIENT-ID"], "cid-test");
});

test("a user token pins no-store and forwards as Bearer", async () => {
  const res = await run({
    query: { path: "users/@me" },
    headers: { "x-mal-user-token": "USERTOK" },
  });
  assert.equal(res.statusCode, 200);
  assert.equal(res.headers["cache-control"], "no-store");
  assert.equal(upstreamCalls[0].init.headers.Authorization, "Bearer USERTOK");
});

test("personal paths without a token are refused", async () => {
  const res = await run({ query: { path: "users/@me/mangalist" } });
  assert.equal(res.statusCode, 401);
  assert.equal(upstreamCalls.length, 0);
});

test("paths outside the allowlist are refused", async () => {
  const res = await run({ query: { path: "users/@me/../../v1/oauth2/token" } });
  assert.equal(res.statusCode, 400);
  assert.equal(upstreamCalls.length, 0);
});

test("PATCH is only my_list_status, only with a user token, only safe fields", async () => {
  const noToken = await run({ method: "PATCH", query: { path: "manga/42/my_list_status" } });
  assert.equal(noToken.statusCode, 401); // right path, missing auth

  const wrongPath = await run({
    method: "PATCH",
    query: { path: "manga/42" },
    headers: { "x-mal-user-token": "USERTOK" },
  });
  assert.equal(wrongPath.statusCode, 405);

  const ok = await run({
    method: "PATCH",
    query: { path: "manga/42/my_list_status" },
    headers: { "x-mal-user-token": "USERTOK" },
    body: { status: "reading", num_chapters_read: 7, score: 10, evil: "x" },
  });
  assert.equal(ok.statusCode, 200);
  const sent = upstreamCalls[0].init.body.toString();
  assert.equal(sent, "status=reading&num_chapters_read=7"); // score/evil dropped
});

test("PATCH with nothing writable is refused before any upstream call", async () => {
  const res = await run({
    method: "PATCH",
    query: { path: "manga/42/my_list_status" },
    headers: { "x-mal-user-token": "USERTOK" },
    body: { evil: "only" },
  });
  assert.equal(res.statusCode, 400);
  assert.equal(upstreamCalls.length, 0);
});
