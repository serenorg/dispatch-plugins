# channel-schema

Shared manifest and catalog types for the Dispatch channel plugins in this
repository.

Channel plugins import `dispatch-channel-protocol` directly for the JSONL wire
contract. This crate keeps the repo-local schema that is still shared across
multiple crates:

- install/runtime protocol basics
- capability declarations
- secret and network requirements
- ingress/path hints for channel plugins
- local catalog indexing for extension discovery

## License

MIT
