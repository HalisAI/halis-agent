# Halis Agent

```
   __  __    _    ___ _   _
  |  \/  |  / \  |_ _| \ | |
  | |\/| | / _ \  | ||  \| |
  | |  | |/ ___ \ | || |\  |
  |_|  |_/_/   \_\___|_| \_|
```

A coding agent for your terminal, written in Rust.

Halis reads and writes files, runs shell commands, and answers questions about
your codebase by driving a language model through a tool loop. This repository
currently contains the agent core: the agent loop, the model clients and the
tool suite. A terminal UI is planned for a later release.

## Status

Working foundation, pre-1.0:

- Agent loop with streaming output, tool calls, retries and interruption
- Model clients for the three major wire protocols
- 8 built-in tools
- Interactive REPL and headless one-shot mode
- Session logging and resume
- Model catalog with 8,400+ chat models from 235 providers (via [models.dev](https://models.dev))

## Install

Requires Rust 1.99.1 (pinned in `rust-toolchain.toml`).

```
git clone https://github.com/HalisAI/halis-agent.git
cd halis-agent
cargo build --release
# binary at target/release/halis
```

## Quick start

```
export ANTHROPIC_API_KEY=sk-...        # or OPENAI_API_KEY, GOOGLE_API_KEY, ...
halis "explain the build failure in this repo"
halis                                  # interactive session
```

The default model is picked from the first provider with a key in the
environment. Override it with `--model`:

```
halis -m anthropic/claude-sonnet-4-5 "review src/agent.rs"
halis -m gpt-5.4-mini                  # bare ids work when unambiguous
halis -m grok-4.7 --effort high "profile this query"
```

Model specs accept `provider/model`, a bare model id, or a unique substring;
ambiguous input lists the candidates.

### Headless mode

Pass a prompt as an argument for scripting. Without a terminal on stdin,
mutating tools are denied unless you pass `--yolo`:

```
halis --yolo "fix the failing test and run cargo test"
```

### REPL commands

| Command | Action |
| --- | --- |
| `/model [spec]` | show or switch the active model |
| `/models` | list catalog models |
| `/tools` | list available tools |
| `/usage` | session token totals |
| `/clear` | start a fresh session |
| `/exit` | quit |

`Ctrl-C` interrupts the running turn; press it while idle to exit.

## Models

The embedded catalog is generated from the [models.dev](https://models.dev)
dataset (`scripts/build_catalog.py`, output committed at
`crates/halis-core/src/catalog/data.json`). It carries context windows, output
limits, tool/vision/reasoning capabilities and per-million pricing for every
chat model, so Halis can compute turn cost locally.

```
halis models --provider anthropic
halis models --search "sonnet-4-5"
```

Columns: `T/V/R/!` = tools / vision / reasoning / deprecated, `*` = api key
detected in the environment.

Supported protocols: OpenAI-compatible chat completions (OpenAI, xAI, Groq,
DeepSeek, Mistral, OpenRouter, Together, Cerebras, Perplexity, Cohere and
hundreds of compatible gateways), Anthropic messages, and Google
generate-content. Providers that need account-scoped setup (Bedrock, Azure,
Vertex) are listed but not callable yet.

### API keys

Keys are read from each provider's documented environment variable, e.g.
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`/`GOOGLE_API_KEY`,
`XAI_API_KEY`, `GROQ_API_KEY`, `DEEPSEEK_API_KEY`, `OPENROUTER_API_KEY`, ...
Run `halis models` to see the variable names for a provider.

### Custom providers

Any OpenAI-compatible endpoint (Ollama, vLLM, LM Studio, gateways) can be
wired up in `config.toml`:

```toml
[providers.ollama]
kind = "openai"
base_url = "http://localhost:11434/v1"

[providers.mygateway]
kind = "openai"
base_url = "https://gw.internal/v1"
api_key = "..."
```

```
halis -m ollama/qwen3-coder "fix the bug in parser.rs"
```

## Tools

| Tool | Kind | Purpose |
| --- | --- | --- |
| `bash` | mutating | run a command (timeout, output caps, interruption) |
| `read_file` | read | file contents with line numbers, offset/limit paging |
| `write_file` | mutating | create or overwrite, creates parent dirs |
| `edit_file` | mutating | exact-match replace, uniqueness enforced |
| `glob` | read | find files by glob pattern |
| `grep` | read | regex content search with `path:line:text` results |
| `ls` | read | directory listing |

Search tools respect `.gitignore` and skip hidden files and binaries. Tool
output is truncated (read: 2000 lines / 2000 chars per line; bash: 24k chars
with middle truncation) so context windows stay manageable.

### Approvals

Read-only tools run freely. Mutating tools (`bash`, `write_file`,
`edit_file`) ask for confirmation in interactive sessions:

```
approve: bash command=cargo test -- --nocapture
[y]es / (n)o / (a)lways:
```

`always` approves the rest of the session. `--yolo` or `approval = "auto"` in
config skips approvals entirely.

## Configuration

`~/.config/halis/config.toml` (override with `HALIS_CONFIG`):

```toml
model = "anthropic/claude-sonnet-4-5"
approval = "confirm"      # or "auto"
max_turns = 40
effort = "medium"         # low | medium | high, for models that support it
bash_timeout_secs = 120

[providers.openai]        # overrides for a catalog provider
base_url = "https://proxy.internal/v1"
api_key = "sk-..."

[providers.ollama]        # custom provider
kind = "openai"
base_url = "http://localhost:11434/v1"
```

Environment variables:

| Variable | Purpose |
| --- | --- |
| `HALIS_MODEL` | default model spec |
| `HALIS_CONFIG` | config file path |
| `HALIS_DATA` | data directory (sessions) |
| `NO_COLOR` | disable colored output |

## Sessions

Every turn is appended to `~/.local/share/halis/sessions/<id>.jsonl`
(messages, tool results, usage, turn stats). Resume with:

```
halis -C                          # continue the latest session
halis -s 20261108-101530          # resume by id prefix
```

## Architecture

```
crates/
├── halis-core/           # the agent engine
│   └── src/
│       ├── agent.rs      # turn loop: stream → tool calls → feedback → repeat
│       ├── message.rs    # unified message model (text/thinking/tool blocks)
│       ├── provider/     # wire protocols
│       │   ├── openai.rs     # chat completions + SSE
│       │   ├── anthropic.rs  # messages API, thinking, tool use
│       │   └── google.rs     # generate-content + SSE
│       ├── catalog.rs    # embedded models.dev catalog + resolution
│       ├── resolver.rs   # spec → provider/protocol/base_url/api key
│       ├── tools/        # bash, fs, search tools behind a registry
│       ├── session.rs    # JSONL persistence and resume
│       ├── sse.rs        # incremental SSE decoder
│       └── config.rs     # config file and env handling
└── halis/                # the CLI binary (REPL, approvals, rendering)
```

The core is UI-agnostic: it emits typed events over a channel, so the planned
TUI can replace the line-based frontend without touching the engine.

### Retry and streaming behavior

Model requests retry up to 4 attempts with exponential backoff (0.5s → 4s,
jittered) on 429/5xx and connection errors, honoring `Retry-After` when
present. Streaming responses are parsed incrementally; a `Ctrl-C` cancels the
in-flight request and preserves partial output in the session. Usage and cost
are accumulated per turn and per session from provider-reported token counts.

## Development

```
cargo test --workspace       # 41 tests incl. an end-to-end agent loop
cargo clippy --workspace
python3 scripts/build_catalog.py   # regenerate the model catalog
```

## Roadmap

- Terminal UI (streaming, diffs, approval panes)
- Context compaction for long sessions
- Parallel tool execution
- Sandbox modes for shell commands
- OpenAI responses protocol
- MCP server support

## License

MIT — see [LICENSE](LICENSE).

Model catalog data comes from [models.dev](https://models.dev) and is
regenerated with `scripts/build_catalog.py`.
