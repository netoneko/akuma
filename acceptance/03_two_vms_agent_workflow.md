# Two VM agent workflow

The goal is to to launch 2 qemu vms, one with `meow` and another one with llama.cpp and prove that `meow` can compile hello.c with minimal amount of memory.


## configuration

### meow VM

* `MEMORY=64 cargo run --release`, etc
* `meow` should be installed via `./scripts/populate-disk.sh`
* `/etc/meow/config` base url should be updated to point to `llama.cpp` vm
* `meow` should be executed with a command `cd akuma-playground && meow -c "compile /akuma-playground/hello.c with tcc and verify that it runs and returns a greeting, write a report to /tmp/tcc_hello_c.md"

### llama.cpp VM

* `MEMORY=4096`
* download an example model with curl (unless model.gguf is present in `./bootstrap/`): `curl -L "https://huggingface.co/unsloth/Qwen3.5-0.8B-GGUF/resolve/main/Qwen3.5-0.8B-Q4_K_M.gguf?download=true" -o qwen3.5-0.8b-q4.gguf`
* `llama.cpp` installation: `apk add llama.cpp llama.cpp-server --repository=http://dl-cdn.alpinelinux.org/alpine/edge/testing/`
* ip of this vm should be used as the ip in the meow config

