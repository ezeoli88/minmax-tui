# MinMax TUI

> A terminal-native AI coding assistant powered by MiniMax — think Cursor, but in your terminal.

<p align="center">
  <img src="https://img.shields.io/badge/runtime-Bun-f472b6?style=flat-square&logo=bun" />
  <img src="https://img.shields.io/badge/UI-Ink%20(React)-61dafb?style=flat-square&logo=react" />
  <img src="https://img.shields.io/badge/LLM-MiniMax%20M2.5-7c3aed?style=flat-square" />
  <img src="https://img.shields.io/badge/license-MIT-22c55e?style=flat-square" />
</p>

---

<img width="1479" height="715" alt="Captura de pantalla 2026-02-20 150635" src="https://github.com/user-attachments/assets/88e6981f-7601-40a5-a8af-5c2ddf6e2b17" />


## What is this?

MinMax TUI is a fully interactive AI chat interface that runs in your terminal. It connects to MiniMax's reasoning models and gives the AI direct access to your filesystem — reading, writing, editing files, running commands, and more — all through a clean, themed terminal UI.

**Key highlights:**

- **Agentic loop** — the AI can chain multiple tool calls autonomously to complete complex tasks
- **PLAN / BUILDER modes** — switch between read-only analysis and full execution with `Tab`
- **Session persistence** — conversations are saved in SQLite and can be resumed anytime
- **MCP support** — extend the AI's capabilities by connecting external MCP servers
- **3 built-in themes** — Tokyo Night, Rosé Pine, and Gruvbox
- **Single binary** — compile to a standalone executable with `bun build`

---

## Quick Start

### Prerequisites

- [Bun](https://bun.sh) v1.1+
- A MiniMax API key — get one free at [platform.minimaxi.com](https://platform.minimaxi.com)

### Install & Run

```bash
# Clone the repo
git clone https://github.com/ezeoli88/minmax-tui.git
cd minmax-tui

# Install dependencies
bun install

# Run
bun start
```

On first launch you'll be prompted to enter your API key. It's saved locally at `~/.minmax-terminal/config.json`.

### Build standalone binary

```bash
bun run build
./minmax        # Linux/macOS
./minmax.exe    # Windows
```

---

## Usage

### Modes

Toggle between modes with **Tab**:

| Mode | Prompt | Description |
|------|--------|-------------|
| **BUILDER** | `build>` | Full access — the AI can read, write, edit files, and run commands |
| **PLAN** | `plan>` | Read-only — the AI can only analyze and suggest, not modify anything |

### Commands

Type `/` to open the command palette, or enter commands directly:

| Command | Description |
|---------|-------------|
| `/new` | Start a new chat session |
| `/sessions` | Browse and resume previous sessions |
| `/config` | Open settings (API key, theme, model) |
| `/model` | Switch between available models |
| `/theme` | Change the color theme |
| `/init` | Create an `agent.md` template in the current directory |
| `/clear` | Clear the current chat |
| `/help` | Show all commands |
| `/exit` | Quit |

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Tab` | Toggle PLAN / BUILDER mode |
| `Esc` | Cancel current AI response |
| `Up/Down` | Scroll (3 lines) |
| `Ctrl+U / Ctrl+D` | Scroll half page |
| Mouse wheel | Scroll |

---

## Built-in Tools

The AI has access to these tools for interacting with your system:

| Tool | What it does |
|------|-------------|
| `bash` | Execute shell commands (30s timeout) |
| `read_file` | Read file contents with optional line ranges |
| `write_file` | Create or overwrite files |
| `edit_file` | Find-and-replace exact strings in files |
| `glob` | Find files by pattern (`**/*.ts`) |
| `grep` | Search file contents with regex |
| `list_directory` | List directory tree with sizes |

In **PLAN** mode, only read-only tools (`read_file`, `glob`, `grep`, `list_directory`) are available.

---

## Models

| Model | Speed | Best for |
|-------|-------|----------|
| `MiniMax-M2.5` | ~60 tok/s | Complex reasoning, detailed analysis |
| `MiniMax-M2.5-highspeed` | ~100 tok/s | Quick iterations, simple tasks |

Switch models with `/model <name>` or through `/config`.

---

## Themes

Three built-in color themes. Switch with `/theme <name>`:

- **tokyo-night** (default) — cool blues and purples
- **rose-pine** — soft pinks and muted tones
- **gruvbox** — warm retro palette

---

## `agent.md`

Drop an `agent.md` file in your project root to give the AI persistent context about your project. It's automatically included in the system prompt.

```bash
# Generate a template
/init
```

Example:

```markdown
# Agent Instructions

## Project Description
A REST API for managing tasks built with Express and PostgreSQL.

## Tech Stack
- Language: TypeScript
- Framework: Express.js
- Database: PostgreSQL with Prisma

## Coding Conventions
- Use camelCase for variables and functions
- All endpoints return JSON with { data, error } shape
- Write tests for every new endpoint
```

---

## MCP Integration

Extend the AI's capabilities by connecting [Model Context Protocol](https://modelcontextprotocol.io) servers. Add them to your config:

```jsonc
// ~/.minmax-terminal/config.json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {
        "GITHUB_TOKEN": "ghp_..."
      }
    }
  }
}
```

MCP tools appear to the AI prefixed as `mcp__servername__toolname`.

---

## Configuration

All config is stored at `~/.minmax-terminal/config.json`:

```json
{
  "apiKey": "your-minimax-api-key",
  "model": "MiniMax-M2.5",
  "theme": "tokyo-night",
  "mcpServers": {}
}
```

Sessions are persisted in `~/.minmax-terminal/sessions.db` (SQLite).

---

## Project Structure

```
src/
  index.ts               # Entry point
  app.tsx                 # Root component
  core/
    api.ts               # MiniMax streaming client
    parser.ts            # XML output parser (<think>, tool calls)
    tools.ts             # Tool registry and execution
    mcp.ts               # MCP server management
    commands.ts          # Slash command handler
    session.ts           # SQLite session persistence
  hooks/
    useChat.ts           # Chat state + agentic tool loop
    useSession.ts        # Session CRUD
    useMode.ts           # PLAN/BUILDER toggle
    useQuota.ts          # API quota polling
    useMouseScroll.ts    # Mouse wheel support
  components/            # Ink (React) terminal UI components
  tools/                 # Built-in tool implementations
  config/                # Settings and theme definitions
```

For a deep dive, see [arquitectura.md](./arquitectura.md).

---

## Development

```bash
bun dev     # Run with --watch (hot reload)
bun start   # Run normally
bun build   # Compile to standalone binary
```

---

## License

MIT
