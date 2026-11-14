// Run with `node --test` in mcp/. The docs come from a small fixture, or from a real
// pages.json when DOCS_FILE names one (as built by `cargo run -p cadmus-site`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import server from "../src/index.js";

const fixture = {
  version: "9.9.9",
  pages: [
    {
      slug: "models", title: "Models", section: "Basics", summary: "Describe your data.",
      url: "https://example.test/docs/models.html", markdown: "Models are structs.\n\n## Foreign keys\n\nPoint at another model.",
      sections: [
        { heading: "Models", anchor: "", text: "Models are structs." },
        { heading: "Foreign keys", anchor: "#foreign-keys", text: "Point at another model." },
      ],
    },
    {
      slug: "deploying", title: "Deploying", section: "Running", summary: "Put it on a server.",
      url: "https://example.test/docs/deploying.html", markdown: "Use Docker.",
      sections: [{ heading: "Deploying", anchor: "", text: "Use Docker." }],
    },
  ],
};
const docs = process.env.DOCS_FILE ? JSON.parse(readFileSync(process.env.DOCS_FILE, "utf8")) : fixture;
globalThis.fetch = async () => new Response(JSON.stringify(docs), { headers: { "Content-Type": "application/json" } });

let nextId = 1;
async function rpc(method, params, headers = {}) {
  const response = await server.fetch(
    new Request("https://mcp.test/mcp", {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json, text/event-stream", ...headers },
      body: JSON.stringify({ jsonrpc: "2.0", id: nextId++, method, params }),
    }),
    {},
  );
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("Access-Control-Allow-Origin"), "*");
  return response.json();
}
async function tool(name, args) {
  const reply = await rpc("tools/call", { name, arguments: args });
  return reply.result;
}

test("initialize negotiates the version and explains the server", async () => {
  const { result } = await rpc("initialize", {
    protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "test", version: "1" },
  });
  assert.equal(result.protocolVersion, "2025-06-18");
  assert.equal(result.serverInfo.name, "cadmus-docs");
  assert.ok(result.capabilities.tools);
  assert.match(result.instructions, /cargo install cadmus-cli/);
  const unknown = await rpc("initialize", { protocolVersion: "1999-01-01" });
  assert.equal(unknown.result.protocolVersion, "2025-11-25");
});

test("the tools are read-only search, fetch and list_pages", async () => {
  const { result } = await rpc("tools/list", {});
  assert.deepEqual(result.tools.map((t) => t.name), ["search", "fetch", "list_pages"]);
  assert.ok(result.tools.every((t) => t.annotations.readOnlyHint));
});

test("search finds sections and points at fetchable pages", async () => {
  const query = process.env.DOCS_FILE ? "foreign key" : "foreign";
  const result = await tool("search", { query });
  const { results } = JSON.parse(result.content[0].text);
  assert.ok(results.length > 0);
  assert.ok(results[0].id && results[0].title && results[0].url.startsWith("http"));
  const none = JSON.parse((await tool("search", { query: "zzqqxx" })).content[0].text);
  assert.deepEqual(none.results, []);
});

test("fetch reads a page by any form of its id", async () => {
  for (const id of ["models", "models.md", "Models.html#foreign-keys", "https://x.test/docs/models.html"]) {
    const page = JSON.parse((await tool("fetch", { id })).content[0].text);
    assert.equal(page.id, "models");
    assert.match(page.text, /^# Models/);
  }
  const missing = await tool("fetch", { id: "nope" });
  assert.equal(missing.isError, true);
});

test("list_pages and resources cover every page", async () => {
  const list = (await tool("list_pages", {})).content[0].text;
  for (const page of docs.pages) assert.ok(list.includes(`- ${page.slug}:`));
  const { result } = await rpc("resources/list", {});
  assert.equal(result.resources.length, docs.pages.length);
  const read = await rpc("resources/read", { uri: result.resources[0].uri });
  assert.match(read.result.contents[0].text, /^# /);
});

test("protocol errors", async () => {
  assert.equal((await rpc("no/such/method", {})).error.code, -32601);
  assert.equal((await rpc("tools/call", { name: "nope", arguments: {} })).error.code, -32602);
  const bad = await server.fetch(new Request("https://mcp.test/mcp", { method: "POST", body: "{not json" }), {});
  assert.equal(bad.status, 400);
  const version = await server.fetch(
    new Request("https://mcp.test/mcp", { method: "POST", headers: { "Mcp-Protocol-Version": "1999-01-01" }, body: "{}" }),
    {},
  );
  assert.equal(version.status, 400);
});

test("notifications get 202, streams 405, browsers a page, preflight 204", async () => {
  const note = await server.fetch(
    new Request("https://mcp.test/mcp", { method: "POST", body: JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) }),
    {},
  );
  assert.equal(note.status, 202);
  const stream = await server.fetch(new Request("https://mcp.test/mcp", { headers: { Accept: "text/event-stream" } }), {});
  assert.equal(stream.status, 405);
  const page = await server.fetch(new Request("https://mcp.test/"), {});
  assert.match(await page.text(), /https:\/\/mcp\.test\/mcp/);
  const preflight = await server.fetch(new Request("https://mcp.test/mcp", { method: "OPTIONS" }), {});
  assert.equal(preflight.status, 204);
  assert.match(preflight.headers.get("Access-Control-Allow-Headers"), /Mcp-Protocol-Version/);
});

test("batches answer each request", async () => {
  const response = await server.fetch(
    new Request("https://mcp.test/mcp", {
      method: "POST",
      body: JSON.stringify([
        { jsonrpc: "2.0", id: 1, method: "ping" },
        { jsonrpc: "2.0", method: "notifications/initialized" },
        { jsonrpc: "2.0", id: 2, method: "tools/list" },
      ]),
    }),
    {},
  );
  const replies = await response.json();
  assert.deepEqual(replies.map((r) => r.id), [1, 2]);
});
