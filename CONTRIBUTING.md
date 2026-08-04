# Contribuir

Gracias por tu interés en `nopaldb-mcp`.

- **Issues y PRs de este servidor MCP** (tools, transportes, auth, docs): aquí.
- **Issues del engine** (NQL, storage, índices, algoritmos, Python bindings):
  van en el repo del engine, [sharop/nopaldb](https://github.com/sharop/nopaldb/issues).

## Desarrollo

```bash
cargo build
cargo test
cargo clippy -- -D warnings
```

El toolchain está fijado en `rust-toolchain.toml`. Las contribuciones se
licencian bajo AGPL-3.0-only (ver `LICENSE`).
