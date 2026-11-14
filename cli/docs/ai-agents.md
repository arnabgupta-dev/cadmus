---
title: Building with AI agents
section: Building with AI agents
order: 200
summary: Connect Claude Code, Cursor, VS Code or any MCP client to the cadmus MCP server, so an agent can read these docs and build Cadmus apps.
---

Cadmus ships with an [MCP](https://modelcontextprotocol.io) server, so AI coding agents can
build on it the way you would: read these docs, create a project, add models, make and apply
migrations. It is part of the `cadmus` command, and runs on your machine.

```sh
cargo install cadmus-cli     # if you have not already
cadmus mcp                   # what your agent runs; you do not run it yourself
```

## Connecting your agent

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

## What the agent can do

| Tool | What it does |
|---|---|
| `docs_list` | lists these documentation pages |
| `docs_read` | reads one page, as Markdown |
| `docs_search` | finds the sections that match some words |
| `new_project` | creates a project in a folder |
| `generate_model` | adds a model with its fields |
| `run` | runs a project command: `makemigrations`, `migrate`, `showmigrations`, `sqlmigrate`, `check`, `routes`, `openapi` or `apikey` |
| `create_superuser` | creates an account to log in with |

The documentation pages are also offered as MCP resources (`cadmus://docs/<page>`), and the
server tells the agent the usual order of work when it connects.

`run` only runs those commands, through `cargo run` in the project folder; it cannot run
anything else. `makemigrations` runs without asking questions there, so nothing is taken as a
rename: if a field was renamed, the agent should mark it with `#[sea_orm(renamed_from = "...")]`
first. The agent does not start the server; it tells you to run `cargo run -- serve`.

## Asking for an app

Once connected, describe what you want:

> Create a Cadmus project called `library` in ~/code with books, authors and loans. Books
> have a title, an ISBN and an author; loans link a book to a member's name with a due date.
> Show books by title in the admin, searchable by title and ISBN, filterable by author.

The agent creates the project, generates the models in the right order, sets the admin
options, makes and applies the migration, and creates an account for you.

## AGENTS.md

Every project made by `cadmus new` has an `AGENTS.md` file: a short guide to the project for
agents, which many of them read automatically. It works without the MCP server too.

## For other tools

This site also offers [llms.txt](https://arnabgupta-dev.github.io/cadmus/llms.txt), an index
of these pages, and
[llms-full.txt](https://arnabgupta-dev.github.io/cadmus/llms-full.txt), all of them in one
file, for tools that read documentation from the web.
