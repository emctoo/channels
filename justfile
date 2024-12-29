default:
    @just --list

# Auto-format the source tree
fmt:
    treefmt

# Run 'cargo run' on the project
run *ARGS:
    cargo run {{ARGS}}

# Run 'cargo watch' to run the project (auto-recompiles)
watch *ARGS:
    cargo watch -x "run -- {{ARGS}}"


# run example server
server:
    watchexec -w . -r -n -c -- RUST_LOG=debug cargo run --example server

channel target="axum":
  watchexec -w . -e rs -r -- RUST_LOG=debug cargo run --bin channel-{{target}} -- \
    --host 0.0.0.0 --port 5000 --redis-url redis://192.168.11.37:6379 --redis-topic streaming:data

pub:
  redli --uri redis://192.168.11.37:6379 publish streaming:default '{"type": "message", "message": "hello, world!+"}'

