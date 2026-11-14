// The Cadmus documentation as a remote MCP server, for any AI app that can add one by URL:
// ChatGPT, Claude, Gemini CLI, Cursor, VS Code and others.
//
// It speaks MCP's Streamable HTTP transport, statelessly: every request is a JSON-RPC POST and
// gets a JSON reply. It is read-only and needs no login. The docs come from `pages.json` on the
// website (built by `site/`), cached for a few minutes, so publishing the site updates the
// server without redeploying it.
//
// Written for Cloudflare Workers, but it uses only standard web APIs (fetch, Request,
// Response), so it runs on any host with the same interface (Deno, Bun, Vercel Edge).

const DEFAULT_DOCS_URL = "https://arnabgupta-dev.github.io/cadmus/docs/pages.json";
const PROTOCOL_VERSIONS = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER_VERSION = "1.0.0";
const CACHE_MS = 5 * 60 * 1000;
const MAX_BODY = 1 << 20;

const INSTRUCTIONS = `This server holds the documentation of Cadmus, a batteries-included web framework for Rust (axum + SeaORM): models become database tables, an admin site and a REST API with documentation, with accounts, permissions and migrations built in.

Use it before writing Cadmus code:
1. search with a few words (for example "foreign key", "custom route", "deploy docker") to find the right page.
2. fetch a page by its id (the slug, such as "models") to read all of it. Links inside pages like "relations.md" are other ids.
3. list_pages shows every page, in reading order.

To build a project, install the CLI with "cargo install cadmus-cli" and start with "cadmus new <name>". The "Getting started" (getting-started) and "Models" (models) pages cover the basics; "AI agents" (ai-agents) explains the local MCP server that can also create projects and run commands.`;

const READ_ONLY = { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false };

const TOOLS = [
  {
    name: "search",
    title: "Search the Cadmus docs",
    description:
      "Search the Cadmus documentation. Returns the best matching sections, each with the id of its page for fetch.",
    inputSchema: {
      type: "object",
      properties: { query: { type: "string", description: "A few words, such as \"many to many\" or \"api keys\"." } },
      required: ["query"],
      additionalProperties: false,
    },
    annotations: READ_ONLY,
  },
  {
    name: "fetch",
    title: "Read a Cadmus docs page",
    description:
      "Read a whole page of the Cadmus documentation, as Markdown, by its id (the slug from search or list_pages, such as \"models\").",
    inputSchema: {
      type: "object",
      properties: { id: { type: "string", description: "The page id, such as \"getting-started\"." } },
      required: ["id"],
      additionalProperties: false,
    },
    annotations: READ_ONLY,
  },
  {
    name: "list_pages",
    title: "List the Cadmus docs pages",
    description: "List every page of the Cadmus documentation in reading order, with its id and a one-line summary.",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    annotations: READ_ONLY,
  },
];

let cache = null;

async function loadDocs(env) {
  if (cache && Date.now() - cache.at < CACHE_MS) return cache.docs;
  try {
    const response = await fetch(env.DOCS_URL || DEFAULT_DOCS_URL, { cf: { cacheTtl: 300 } });
    if (!response.ok) throw new Error(`the docs could not be loaded (HTTP ${response.status})`);
    const docs = await response.json();
    if (!docs || !Array.isArray(docs.pages)) throw new Error("the docs file has no pages");
    cache = { at: Date.now(), docs };
    return docs;
  } catch (error) {
    // Serving the last copy beats failing while the website is briefly unreachable.
    if (cache) return cache.docs;
    throw error;
  }
}

/** Accepts "models", "models.md", "models.html#fields" or a full docs address. */
function pageId(raw) {
  let id = String(raw).trim().split("#")[0].split("?")[0];
  id = id.slice(id.lastIndexOf("/") + 1);
  return id.replace(/\.(md|html)$/, "").toLowerCase();
}

const STOP_WORDS = new Set(
  "a an and are as at be by can do does for from how i in is it my of on or the to use using what when with".split(" "),
);

/** Lower case, with punctuation as spaces, so "many-to-many" matches "many to many". */
function normalize(value) {
  return ` ${value.toLowerCase().replace(/[^a-z0-9]+/g, " ").trim()} `;
}

function search(docs, query) {
  const words = normalize(query).trim().split(" ").filter(Boolean);
  const meaningful = words.filter((t) => t.length > 1 && !STOP_WORDS.has(t));
  const terms = meaningful.length ? meaningful : words;
  if (!terms.length) return [];
  // The whole query as a phrase counts extra, so "many to many" prefers that exact phrase.
  const phrase = words.length > 1 ? ` ${words.join(" ")} ` : null;
  const hits = [];
  for (const page of docs.pages) {
    for (const part of page.sections || []) {
      const heading = normalize(part.heading);
      const title = normalize(page.title);
      const text = normalize(part.text);
      let score = 0;
      for (const term of terms) {
        let found = false;
        if (heading.includes(term)) { score += 10; found = true; }
        if (title.includes(term)) { score += 4; found = true; }
        if (text.includes(term)) { score += 1 + Math.min(5, text.split(term).length - 1); found = true; }
        if (!found) { score = 0; break; }
      }
      if (score > 0 && phrase && (heading + text).includes(phrase)) score += 15;
      if (score > 0) hits.push({ page, part, score });
    }
  }
  hits.sort((a, b) => b.score - a.score);
  return hits.slice(0, 8).map(({ page, part }) => ({
    id: page.slug,
    title: part.heading === page.title ? page.title : `${page.title} › ${part.heading}`,
    url: page.url + (part.anchor || ""),
    text: snippet(part.text),
  }));
}

function snippet(markdown) {
  const flat = markdown.replace(/```[\s\S]*?```/g, " ").replace(/[`*#>|]/g, " ").replace(/\s+/g, " ").trim();
  return flat.length > 300 ? `${flat.slice(0, 300)}…` : flat;
}

function text(value) {
  return { content: [{ type: "text", text: typeof value === "string" ? value : JSON.stringify(value) }] };
}

function toolError(message) {
  return { content: [{ type: "text", text: message }], isError: true };
}

async function callTool(params, env) {
  const name = params && params.name;
  const args = (params && params.arguments) || {};
  const docs = await loadDocs(env);
  switch (name) {
    case "search": {
      if (typeof args.query !== "string" || !args.query.trim()) return toolError("Give a query: a few words to look for.");
      const results = search(docs, args.query.slice(0, 200));
      return text({ results });
    }
    case "fetch": {
      if (typeof args.id !== "string" || !args.id.trim()) return toolError("Give the id of a page, such as \"models\".");
      const id = pageId(args.id);
      const page = docs.pages.find((p) => p.slug === id);
      if (!page) return toolError(`There is no page "${id}". Use list_pages to see every id.`);
      return text({
        id: page.slug,
        title: page.title,
        text: `# ${page.title}\n\n${page.markdown}`,
        url: page.url,
        metadata: { section: page.section, summary: page.summary, version: docs.version },
      });
    }
    case "list_pages": {
      let out = `Cadmus ${docs.version} documentation. Read a page with fetch and its id.\n`;
      let section = "";
      for (const page of docs.pages) {
        if (page.section !== section) {
          section = page.section;
          out += `\n${section}\n`;
        }
        out += `- ${page.slug}: ${page.title}. ${page.summary}\n`;
      }
      return text(out);
    }
    default:
      throw rpcError(-32602, `Unknown tool: ${name}`);
  }
}

function rpcError(code, message) {
  const error = new Error(message);
  error.rpc = { code, message };
  return error;
}

async function dispatch(message, env) {
  const params = message.params || {};
  switch (message.method) {
    case "initialize": {
      const asked = params.protocolVersion;
      return {
        protocolVersion: PROTOCOL_VERSIONS.includes(asked) ? asked : PROTOCOL_VERSIONS[0],
        capabilities: { tools: { listChanged: false }, resources: { listChanged: false } },
        serverInfo: {
          name: "cadmus-docs",
          title: "Cadmus documentation",
          version: SERVER_VERSION,
          websiteUrl: "https://arnabgupta-dev.github.io/cadmus/",
        },
        instructions: INSTRUCTIONS,
      };
    }
    case "ping":
      return {};
    case "tools/list":
      return { tools: TOOLS };
    case "tools/call":
      return callTool(params, env);
    case "resources/list": {
      const docs = await loadDocs(env);
      return {
        resources: docs.pages.map((page) => ({
          uri: `cadmus://docs/${page.slug}`,
          name: page.slug,
          title: page.title,
          description: page.summary,
          mimeType: "text/markdown",
        })),
      };
    }
    case "resources/read": {
      const docs = await loadDocs(env);
      const uri = String(params.uri || "");
      const page = docs.pages.find((p) => `cadmus://docs/${p.slug}` === uri);
      if (!page) throw rpcError(-32002, `Resource not found: ${uri}`);
      return { contents: [{ uri, mimeType: "text/markdown", text: `# ${page.title}\n\n${page.markdown}` }] };
    }
    case "resources/templates/list":
      return { resourceTemplates: [] };
    case "prompts/list":
      return { prompts: [] };
    default:
      throw rpcError(-32601, `Method not found: ${message.method}`);
  }
}

/** Answers one JSON-RPC message; notifications and responses get no answer (null). */
async function answer(message, env) {
  if (!message || typeof message !== "object" || message.jsonrpc !== "2.0") {
    return { jsonrpc: "2.0", id: null, error: { code: -32600, message: "Invalid request" } };
  }
  if (message.id === undefined || message.id === null || typeof message.method !== "string") return null;
  try {
    return { jsonrpc: "2.0", id: message.id, result: await dispatch(message, env) };
  } catch (error) {
    const rpc = error.rpc || { code: -32603, message: `Internal error: ${error.message}` };
    return { jsonrpc: "2.0", id: message.id, error: rpc };
  }
}

const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, DELETE, OPTIONS",
  "Access-Control-Allow-Headers":
    "Content-Type, Accept, Authorization, Mcp-Session-Id, Mcp-Protocol-Version, Last-Event-ID",
  "Access-Control-Expose-Headers": "Mcp-Session-Id, Mcp-Protocol-Version",
  "Access-Control-Max-Age": "86400",
};

function json(body, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json", ...CORS },
  });
}

function empty(status, extra = {}) {
  return new Response(null, { status, headers: { ...CORS, ...extra } });
}

function infoPage(url) {
  const endpoint = `${url.origin}/mcp`;
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Cadmus docs MCP server</title>
<style>:root{color-scheme:light dark}body{font:16px/1.6 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}code{background:rgba(127,127,127,.15);padding:.1em .35em;border-radius:4px}</style></head>
<body><h1>Cadmus docs MCP server</h1>
<p>This address is for AI apps, not browsers. Add it as a remote MCP server (no login needed):</p>
<p><code>${endpoint}</code></p>
<p>It lets an assistant search and read the <a href="https://arnabgupta-dev.github.io/cadmus/">Cadmus</a> documentation. Setup steps for each app are on the <a href="https://arnabgupta-dev.github.io/cadmus/docs/ai-agents.html">AI agents</a> page.</p></body></html>`;
  return new Response(html, { headers: { "Content-Type": "text/html; charset=utf-8", ...CORS } });
}

async function handle(request, env) {
  const url = new URL(request.url);
  if (request.method === "OPTIONS") return empty(204);
  if (url.pathname !== "/mcp" && url.pathname !== "/") return empty(404);

  if (request.method === "GET") {
    // No server-to-client stream is offered; MCP clients expect 405 here.
    if ((request.headers.get("Accept") || "").includes("text/event-stream")) return empty(405, { Allow: "POST" });
    return infoPage(url);
  }
  if (request.method !== "POST") return empty(405, { Allow: "GET, POST" });

  const version = request.headers.get("Mcp-Protocol-Version");
  if (version && !PROTOCOL_VERSIONS.includes(version)) {
    return json({ jsonrpc: "2.0", id: null, error: { code: -32600, message: `Unsupported protocol version: ${version}` } }, 400);
  }
  if (Number(request.headers.get("Content-Length") || 0) > MAX_BODY) {
    return json({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "Request too large" } }, 413);
  }

  let body;
  try {
    const raw = await request.text();
    if (raw.length > MAX_BODY) throw new Error("too large");
    body = JSON.parse(raw);
  } catch {
    return json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } }, 400);
  }

  if (Array.isArray(body)) {
    // Batches are from protocol version 2025-03-26; still answered for older clients.
    const replies = (await Promise.all(body.slice(0, 50).map((m) => answer(m, env)))).filter(Boolean);
    return replies.length ? json(replies) : empty(202);
  }
  const reply = await answer(body, env);
  return reply ? json(reply) : empty(202);
}

export default {
  async fetch(request, env) {
    return handle(request, env || {});
  },
};
