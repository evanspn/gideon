#!/usr/bin/env node
// gideon MCP server (stdio): search the manga catalog and send titles to the
// Kobo through gideon's existing sync backend. See mcp/README.md for setup.

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { GideonClient } from "./lib.js";

const client = new GideonClient();
const server = new McpServer({ name: "gideon-kobo", version: "1.0.0" });

const text = (data) => ({ content: [{ type: "text", text: JSON.stringify(data, null, 2) }] });
const fail = (e) => ({ content: [{ type: "text", text: `Error: ${e.message || e}` }], isError: true });

server.tool(
  "search_manga",
  "Search the manga catalog (MyAnimeList) by title or keywords. Returns titles with community score, year, genres, cover URL, and a synopsis snippet — use a result's title/cover_url with send_to_kobo.",
  { query: z.string().min(2).describe("Title or keywords to search for"), limit: z.number().int().min(1).max(15).optional().describe("Max results (default 8)") },
  async ({ query, limit }) => {
    try {
      return text(await client.searchManga(query, limit ?? 8));
    } catch (e) {
      return fail(e);
    }
  }
);

server.tool(
  "send_to_kobo",
  "Queue a manga title for the Kobo. It appears as a notification bell on the device's Home screen after its next sync; tapping it there searches the device's own manga sources and downloads chapters. Optionally pass a cover_url from search_manga.",
  { title: z.string().min(1).max(512).describe("Manga title (a search term for the device)"), cover_url: z.string().url().optional().describe("Cover art URL for the device notification") },
  async ({ title, cover_url }) => {
    try {
      return text(await client.sendToKobo(title, cover_url));
    } catch (e) {
      return fail(e);
    }
  }
);

server.tool(
  "pending_sends",
  "List titles queued for the Kobo that the device hasn't opened yet.",
  {},
  async () => {
    try {
      return text(await client.pendingSends());
    } catch (e) {
      return fail(e);
    }
  }
);

server.tool(
  "remove_send",
  "Remove a queued title (by id from pending_sends) before the device opens it.",
  { id: z.string().min(1).describe("Send id from pending_sends") },
  async ({ id }) => {
    try {
      return text(await client.removeSend(id));
    } catch (e) {
      return fail(e);
    }
  }
);

server.tool(
  "library",
  "The Kobo's synced reading library: one entry per series with the current chapter, page position, and last-read time. Useful to avoid sending something already on the shelf.",
  {},
  async () => {
    try {
      return text(await client.library());
    } catch (e) {
      return fail(e);
    }
  }
);

await server.connect(new StdioServerTransport());
