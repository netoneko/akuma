# Meow-chan Configuration

Meow-chan Local supports multiple AI providers through a configuration file and interactive setup.

## Quick Start

```bash
# Run interactive setup
meow-local init

# Or just start with defaults (Ollama at localhost:11434)
meow-local
```

## Config File

Location:
- **Linux**: `~/.config/meow/config.toml`
- **macOS**: `~/Library/Application Support/meow/config.toml`

### Example Config

```toml
current_provider = "ollama"
current_model = "gemma3:27b"

[[providers]]
name = "ollama"
base_url = "http://localhost:11434"
api_type = "ollama"

[[providers]]
name = "openai"
base_url = "https://api.openai.com"
api_type = "openai"
api_key = "sk-..."
```

### Fields

| Field | Description |
|-------|-------------|
| `current_provider` | Name of the active provider |
| `current_model` | Currently selected model |
| `providers` | Array of configured providers |

### Provider Fields

| Field | Description |
|-------|-------------|
| `name` | Unique identifier for the provider |
| `base_url` | API endpoint URL |
| `api_type` | Either `ollama` or `openai` |
| `api_key` | Optional API key (required for OpenAI) |

## CLI Commands

### Init Command

```bash
# Interactive setup wizard
meow-local init

# List configured providers
meow-local init --list

# Remove a provider
meow-local init --delete <name>
```

### Runtime Options

```bash
# Use specific provider
meow-local --provider openai

# Override model
meow-local --model gpt-4o

# Combine both
meow-local -p openai -m gpt-4o "Hello"
```

## Interactive Commands

During a chat session:

| Command | Description |
|---------|-------------|
| `/model` | Show current model |
| `/model list` | List available models from current provider |
| `/model <name>` | Switch to a specific model |
| `/provider` | Show current provider |
| `/provider list` | List configured providers |
| `/provider <name>` | Switch to a specific provider |

### Example: Switching Models

```
(=^･ω･^=) > /model list
～ Fetching available models from ollama... ～
～ Available neural links: ～
  1. gemma3:27b [27B]
  2. llama3.2:latest [8B]
  3. codellama:7b [7B]

Enter number to switch (or press Enter to cancel): 2
～ *ears twitch* Neural link reconfigured to: llama3.2:latest nya~! ～
```

### Example: Switching Providers

```
(=^･ω･^=) > /provider list
～ Configured providers: ～
  1. ollama (http://localhost:11434) [Ollama] (current)
  2. openai (https://api.openai.com) [OpenAI]

Enter number to switch (or press Enter to cancel): 2
～ Switched to openai! Fetching models... ～
～ Available models: ～
  1. gpt-4o
  2. gpt-4o-mini

Enter number to select model: 1
～ *ears twitch* Now using openai/gpt-4o nya~! ～
```

## Supported Providers

### Ollama (Default)

- **API Type**: `ollama`
- **Default URL**: `http://localhost:11434`
- **API Key**: Optional
- **Model Listing**: `GET /api/tags`
- **Chat**: `POST /api/chat` (NDJSON streaming)

### OpenAI-Compatible

- **API Type**: `openai`
- **Default URL**: `https://api.openai.com`
- **API Key**: Required
- **Model Listing**: `GET /v1/models`
- **Chat**: `POST /v1/chat/completions` (SSE streaming)

Works with any OpenAI-compatible API:
- OpenAI
- Azure OpenAI
- Local OpenAI-compatible servers (LM Studio, vLLM, etc.)

## Backporting to meow (akuma)

When backporting to the akuma version:

1. **Config Path**: Use `/etc/meow/config.toml` instead of user home directory
2. **TOML Parsing**: Implement simple hand-rolled parser (no serde in no_std)
3. **HTTP**: Provider module can be reused with libakuma's HTTP primitives
4. **Input**: Adapt interactive prompts for akuma's input system

The config format is intentionally simple to make hand-rolled parsing feasible.
