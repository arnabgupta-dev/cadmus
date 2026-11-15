---
title: Building with AI agents
section: Building with AI agents
order: 200
summary: Connect ChatGPT, Claude, Gemini, Cursor, VS Code or any MCP client to Cadmus, so an AI assistant can read these docs or build Cadmus apps for you.
---

Cadmus offers two [MCP](https://modelcontextprotocol.io) servers. MCP is the standard way to
give an AI assistant extra tools; here, the tools are these docs and the `cadmus` command.

## Which one to use

| | Hosted docs server | Local server (`cadmus mcp`) |
|---|---|---|
| **Where it runs** | on the web; nothing to install | on your computer, inside the `cadmus` command |
| **Works with** | any app that adds an MCP server by its address: ChatGPT, Claude (web, desktop and mobile), Claude Code, Gemini CLI, Cursor, VS Code | coding agents on your computer: Claude Code, Claude Desktop, Cursor, VS Code, Gemini CLI |
| **What the assistant can do** | search and read these docs | read these docs, create projects, add models, make and apply migrations, create an account |
| **Use it when** | you want answers and code to paste yourself, or your assistant runs in a browser | you want the agent to build the project for you |

In short: **chatting in a browser, use the hosted server; letting a coding agent build the
app, use the local one.** The local server includes the docs, so you never need both in the
same agent.

## The hosted docs server

Its address:

```text
https://cadmus-mcp.cadmus-mcp.workers.dev/mcp
```

It needs no login, and it can only read the docs. It always serves the docs on this site.

**ChatGPT**: in Settings → Apps & Connectors, create a connector (if there is no button for
it, turn on Developer mode under Advanced settings first). Give it a name, paste the address,
and choose *No authentication*. Custom connectors need a paid ChatGPT plan.

**Claude** (claude.ai, the desktop app and mobile): Settings → Connectors → *Add custom
connector*, then paste the address. It is then available in every conversation where you turn
it on.

**Claude Code**:

```sh
claude mcp add --transport http cadmus-docs https://cadmus-mcp.cadmus-mcp.workers.dev/mcp
```

**Gemini CLI** (`~/.gemini/settings.json`):

```json
{
  "mcpServers": {
    "cadmus-docs": { "httpUrl": "https://cadmus-mcp.cadmus-mcp.workers.dev/mcp" }
  }
}
```

**Cursor** (`~/.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "cadmus-docs": { "url": "https://cadmus-mcp.cadmus-mcp.workers.dev/mcp" }
  }
}
```

**VS Code** (`.vscode/mcp.json` in your workspace):

```json
{
  "servers": {
    "cadmus-docs": { "type": "http", "url": "https://cadmus-mcp.cadmus-mcp.workers.dev/mcp" }
  }
}
```

Other apps: add a remote MCP server (it may be called *Streamable HTTP*) with the address
above.

| Tool | What it does |
|---|---|
| `search` | finds the sections that match some words, with the page each is on |
| `fetch` | reads a whole page, as Markdown |
| `list_pages` | lists every page, in reading order |

Then ask, for example:

> Using the Cadmus docs, show me how to give a `Post` model an author that links to a
> `User`, and how it appears in the admin.

## The local server

The local server is part of the `cadmus` command. Your agent starts it; you do not run it
yourself.

```sh
cargo install cadmus-cli     # if you have not already
```

### Connecting your agent

**Claude Code**

```sh
claude mcp add cadmus -- cadmus mcp
```

**Claude Desktop** (Settings → Developer → Edit Config), **Cursor** (`~/.cursor/mcp.json`)
and other clients that use an `mcpServers` list:

```json
{
  "mcpServers": {
    "cadmus": { "command": "cadmus", "args": ["mcp"] }
  }
}
```

**VS Code** (`.vscode/mcp.json` in your workspace):

```json
{
  "servers": {
    "cadmus": { "type": "stdio", "command": "cadmus", "args": ["mcp"] }
  }
}
```

If the client cannot find `cadmus`, give the full path, usually `~/.cargo/bin/cadmus`.

### What the agent can do

| Tool | What it does |
|---|---|
| `docs_list` | lists these documentation pages |
| `docs_read` | reads one page, as Markdown |
| `docs_search` | finds the sections that match some words |
| `new_project` | creates a project in a folder, with the features it needs (login methods, e-mail, storage) |
| `generate_model` | adds a model with its fields |
| `add_features` | builds in more: a login method such as Google, SMTP, cloud storage or a database driver |
| `delete_model` | removes a model; the next migration drops its table |
| `run` | runs a project command: `makemigrations`, `migrate`, `showmigrations`, `sqlmigrate`, `check`, `routes`, `openapi` or `apikey` |
| `create_superuser` | creates an account to log in with |

The documentation pages are also offered as MCP resources (`cadmus://docs/<page>`), and the
server tells the agent the usual order of work when it connects.

`run` only runs those commands, through `cargo run` in the project folder; it cannot run
anything else. `makemigrations` runs without asking questions there, so nothing is taken as a
rename: if a field was renamed, the agent should mark it with `#[sea_orm(renamed_from = "...")]`
first. The agent does not start the server; it tells you to run `cargo run -- serve`.

### Asking for an app

Once connected, describe what you want:

> Create a Cadmus project called `library` in ~/code with books, authors and loans. Books
> have a title, an ISBN and an author; loans link a book to a member's name with a due date.
> Show books by title in the admin, searchable by title and ISBN, filterable by author.

The agent creates the project, generates the models in the right order, sets the admin
options, makes and applies the migration, and creates an account for you.

## AGENTS.md

Every project made by `cadmus new` has an `AGENTS.md` file: a short guide to the project for
agents, which many of them read automatically. It works without either server.

## For other tools

This site also offers [llms.txt](https://arnabgupta-dev.github.io/cadmus/llms.txt), an index
of these pages, and
[llms-full.txt](https://arnabgupta-dev.github.io/cadmus/llms-full.txt), all of them in one
file, for tools that read documentation from the web.
