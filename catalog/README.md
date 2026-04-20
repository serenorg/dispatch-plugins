# dispatch-extension-catalog

A small local helper for browsing the Dispatch extension catalog in `catalog/extensions.json`.

It is intentionally narrow. It does not install anything itself. It reads the catalog index and provides:

- `list`
- `search <query>`
- `inspect <name>`
- `install-hint <name>`

## Build

```bash
cargo build --release
```

## Usage

From the repository root:

```bash
cargo run --manifest-path catalog/Cargo.toml -- list
cargo run --manifest-path catalog/Cargo.toml -- search slack
cargo run --manifest-path catalog/Cargo.toml -- inspect channel-telegram
```

To point at a different catalog file:

```bash
cargo run --manifest-path catalog/Cargo.toml -- \
  --catalog ./catalog/extensions.json list
```

## License

MIT
