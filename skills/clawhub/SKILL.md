---
name: clawhub
description: "Search, install, and update agent skills from ClawHub using the clawhub CLI via npx."
homepage: https://clawhub.ai
metadata: {"rbot":{"description":"ClawHub marketplace: npx clawhub search/install/update/list; use --workdir for the rbot workspace.","emoji":"🦞","triggers":["clawhub","skill marketplace","install skill","search skills"],"requires":{"bins":["npx"]}}}
---

# ClawHub

[ClawHub](https://clawhub.ai) is a public registry of agent skills. Skills are fetched with the **`clawhub`** CLI, typically through **`npx`** so you do not need a global install.

## When to use

Use this skill when the user wants to discover skills, install or update them, or list what is already installed in the workspace.

## Prerequisites

- **Node.js** (includes `npx`). No API key is required for search, install, or list.
- **`login`** is only needed if the user publishes skills.

## Commands (use `npx clawhub@latest`)

Pin the package so behavior matches current docs:

```bash
npx clawhub@latest <subcommand> ...
```

`--yes` (or your package manager’s non-interactive flag) avoids prompts when appropriate:

```bash
npx --yes clawhub@latest search "web scraping" --limit 5
```

### Search

Natural-language or keyword search:

```bash
npx --yes clawhub@latest search "<query>" --limit 5
```

### Install

Install a skill **into a workspace directory** so rbot can load it (rbot discovers skills under the workspace `skills/` tree—see your rbot config for the exact workspace root).

```bash
npx --yes clawhub@latest install <slug> --workdir /path/to/rbot/workspace
```

Replace `<slug>` with the identifier from search results. **Always set `--workdir`** to the rbot workspace root (the directory that contains `.rbot/` and usually `skills/`), not a random cwd—otherwise files land in the wrong place.

### Update

Refresh installed skills from the registry:

```bash
npx --yes clawhub@latest update --all --workdir /path/to/rbot/workspace
```

### List installed

```bash
npx --yes clawhub@latest list --workdir /path/to/rbot/workspace
```

## rbot-specific notes

- Use the **same `--workdir`** you use for rbot’s configured workspace (often the repo root in development). If unsure, check `agents.defaults.workspace` (or equivalent) in config.
- After installing or updating skills, a **new agent session** may be needed for newly added `skills/<name>/SKILL.md` files to load.
- Publishing is optional and requires `npx clawhub@latest login` once.
