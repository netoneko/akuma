# Configuration for Meow

## Running with ollama

```bash
OLLAMA_LOAD_TIMEOUT=2m OLLAMA_DEBUG=1 OLLAMA_CONTEXT_LENGTH=114688 OLLAMA_KEEP_ALIVE=-1 ollama start
```

Pull gemma3:

```bash
ollama pull gemma3:27b
```

That's it, meow-local will automatically connect to Ollama.

Run meow-local:

```bash
cargo run --release
```
