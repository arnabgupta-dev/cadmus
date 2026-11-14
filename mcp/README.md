# Hosted MCP server for the Cadmus docs

A small remote MCP server that lets any AI app that accepts an MCP server URL search and
read the Cadmus documentation: ChatGPT, Claude, Gemini CLI, Cursor, VS Code and others.

- **Transport:** MCP Streamable HTTP, stateless JSON replies, no login. Read-only.
- **Tools:** `search`, `fetch` and `list_pages`. Every page is also a resource,
  `cadmus://docs/<slug>`.
- **Docs source:** `docs/pages.json` on the website, which `cargo run -p cadmus-site` writes.
  The server caches it for 5 minutes, so publishing the site updates the server without a
  redeploy.

This is separate from `cadmus mcp`, the local server in the CLI. The local one also creates
projects and runs commands on your computer; this one only serves the docs.

## Deploy (Cloudflare Workers, free plan)

```sh
cd mcp
npx wrangler@4 login     # opens the browser; sign in or create a free Cloudflare account
npx wrangler@4 deploy
```

The address is `https://cadmus-mcp.<your-subdomain>.workers.dev/mcp`; the live one is
`https://cadmus-mcp.cadmus-mcp.workers.dev/mcp`. Redeploy only when
`src/index.js` changes.

The code uses only standard web APIs, so it can also run on Deno Deploy, Bun or Vercel Edge.

## Test

```sh
node --test                                             # against a small fixture
cargo run -p cadmus-site -- _site                       # from the repository root
DOCS_FILE=../_site/docs/pages.json node --test          # against the real docs
```
