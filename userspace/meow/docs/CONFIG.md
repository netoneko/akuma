# Meow Configuration

Meow stores its configuration at `/etc/meow/config`. This file controls which AI providers are available and which model to use by default.

## Configuration Format

The config file uses a simple key-value format with sections for each provider.

### Example Configuration

```ini
# Global settings
current_provider=ollama
current_model=gemma3:27b

# Default Ollama provider (QEMU host gateway)
[provider:ollama]
base_url=http://10.0.2.2:11434
api_type=ollama

# Alternative: Ollama on a different host
[provider:ollama-remote]
base_url=http://192.168.1.100:11434
api_type=ollama

# OpenAI (HTTPS supported)
[provider:openai]
base_url=https://api.openai.com
api_type=openai
api_key=sk-your-api-key-here

# Groq API (HTTPS supported)
[provider:groq]
base_url=https://api.groq.com/openai/v1
api_type=openai
api_key=gsk_your-groq-key-here
```

## Configuration Options

### Global Settings

| Key | Description | Default |
|-----|-------------|---------|
| `current_provider` | Name of the active provider | `ollama` |
| `current_model` | Model to use for chat | `gemma3:27b` |

### Provider Section

Each provider is defined in a `[provider:name]` section:

| Key | Description | Required |
|-----|-------------|----------|
| `base_url` | HTTP or HTTPS URL of the provider API | Yes |
| `api_type` | API format: `ollama` or `openai` | Yes |
| `api_key` | API key for authentication | No (required for OpenAI) |

## Provider Types

### Ollama (`api_type=ollama`)

- Uses Ollama's native API format
- Endpoints: `/api/chat`, `/api/tags`, `/api/show`
- No API key required for local instances
- Default port: 11434

### OpenAI (`api_type=openai`)

- Uses OpenAI-compatible API format
- Endpoints: `/v1/chat/completions`, `/v1/models`
- Requires API key via `Authorization: Bearer` header
- Works with OpenAI, Groq, Together.ai, and other compatible providers

## HTTPS Support

Meow fully supports HTTPS connections via TLS 1.3 (using libakuma-tls).

Example configurations:

```ini
# OpenAI
[provider:openai]
base_url=https://api.openai.com
api_type=openai
api_key=sk-your-api-key-here

# Groq
[provider:groq]
base_url=https://api.groq.com/openai/v1
api_type=openai
api_key=gsk_your-groq-key-here

# Anthropic (via OpenAI-compatible proxy)
[provider:anthropic]
base_url=https://api.anthropic.com
api_type=openai
api_key=sk-ant-your-key-here
```

Note: Certificate verification is not yet implemented (NoVerify mode, similar to `curl -k`).

## QEMU Networking

When running in QEMU, the host machine is accessible at `10.0.2.2`:

```ini
# Ollama running on host machine
[provider:ollama]
base_url=http://10.0.2.2:11434
api_type=ollama
```

## Runtime Commands

You can switch providers and models at runtime:

```
/provider              # Show current provider
/provider list         # List all configured providers
/provider openai       # Switch to a specific provider

/model                 # Show current model
/model list            # List models from current provider
/model gpt-4o          # Switch to a specific model

/tokens                # Show current token usage
```

## Creating the Config File

1. Connect to the Akuma kernel via SSH:
   ```bash
   ssh -p 2222 user@localhost
   ```

2. Create the config directory:
   ```bash
   mkdir -p /etc/meow
   ```

3. Create the config file:
   ```bash
   cat > /etc/meow/config << 'EOF'
   current_provider=groq
   current_model=llama-3.3-70b-versatile

   [provider:ollama]
   base_url=http://10.0.2.2:11434
   api_type=ollama

   [provider:groq]
   base_url=https://api.groq.com/openai/v1
   api_type=openai
   api_key=gsk_your-groq-key-here
   EOF
   ```

4. Run meow to verify:
   ```bash
   meow init
   ```

## Viewing Current Configuration

Run `meow init` to see the current configuration:

```
  /\_/\  ╔══════════════════════════════════════╗
 ( o.o ) ║  M E O W - C H A N   I N I T         ║
  > ^ <  ║  ～ Provider Configuration ～        ║
 /|   |\ ╚══════════════════════════════════════╝

～ Current providers: ～
  - ollama [Ollama]: http://10.0.2.2:11434
  - groq [OpenAI]: https://api.groq.com/openai/v1 (current)

  Current model: llama-3.3-70b-versatile
  Config file: /etc/meow/config
```
