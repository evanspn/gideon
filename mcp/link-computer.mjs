#!/usr/bin/env node
// Link this computer to a gideon account without its password ever touching
// this machine's keyboard or any chat. Counterpart of web/link.html:
//
//   node link-computer.mjs            # prints a pairing code (a public key)
//   node link-computer.mjs --finish <link-code>
//
// The phone (signed into gideon-sync.vercel.app) encrypts its session to the
// pairing code; --finish decrypts with the private key held only here and
// writes ~/.config/gideon/mcp-session.json, which GideonClient already uses
// (and refreshes) — no password file needed.
import { webcrypto as wc } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const DIR = path.join(os.homedir(), ".config", "gideon");
const KEY_FILE = path.join(DIR, "link-pending.json");
const SESSION_FILE = path.join(DIR, "mcp-session.json");
const b64 = (buf) => Buffer.from(buf).toString("base64");
const unb64 = (s) => new Uint8Array(Buffer.from(s, "base64"));

fs.mkdirSync(DIR, { recursive: true });

if (process.argv[2] === "--finish") {
  const blob = JSON.parse(Buffer.from(process.argv[3], "base64").toString());
  const pending = JSON.parse(fs.readFileSync(KEY_FILE, "utf8"));
  const priv = await wc.subtle.importKey(
    "pkcs8", unb64(pending.pkcs8), { name: "ECDH", namedCurve: "P-256" }, false, ["deriveKey"]);
  const theirPub = await wc.subtle.importKey(
    "spki", unb64(blob.e), { name: "ECDH", namedCurve: "P-256" }, false, []);
  const aes = await wc.subtle.deriveKey(
    { name: "ECDH", public: theirPub }, priv, { name: "AES-GCM", length: 256 }, false, ["decrypt"]);
  const payload = JSON.parse(
    Buffer.from(await wc.subtle.decrypt({ name: "AES-GCM", iv: unb64(blob.i) }, aes, unb64(blob.c))).toString());
  fs.writeFileSync(SESSION_FILE, JSON.stringify({
    access_token: payload.access_token,
    refresh_token: payload.refresh_token,
  }), { mode: 0o600 });
  fs.rmSync(KEY_FILE, { force: true });
  console.log(`LINKED as ${payload.email || "(unknown email)"} — session saved.`);
} else {
  const kp = await wc.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveKey"]);
  const spki = await wc.subtle.exportKey("spki", kp.publicKey);
  const pkcs8 = await wc.subtle.exportKey("pkcs8", kp.privateKey);
  fs.writeFileSync(KEY_FILE, JSON.stringify({ pkcs8: b64(pkcs8) }), { mode: 0o600 });
  console.log(b64(spki));
}
