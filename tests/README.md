# Integration coverage

The workspace tests exercise domain reducers, Unicode typewriter handling, SQLite
migrations, and strict JSONL framing without needing a model account. `FakeRuntime`
is available for GUI/application integration tests. A real Pi smoke test remains
opt-in because it requires a configured provider:

```sh
PI_WHIM_SMOKE=1 PI_WHIM_PI_BIN=/path/to/pi cargo run -p xtask -- smoke
```
