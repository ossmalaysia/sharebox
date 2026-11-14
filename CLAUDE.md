# Sharebox

Read [AGENTS.md](AGENTS.md) for shared coding standards, validation commands, and release rules. Read [README.md](README.md) for the current architecture and CLI, and [SECURITY.md](SECURITY.md) for the trusted-LAN execution boundary.

The application is a Rust master/worker executable. The dashboard is in `public/`; master and worker implementation is in `src/`. The root `index.html` is an earlier static concept and must not be mistaken for the live product.

Use the existing named-task protocol. Never infer that an MCP connection authorizes arbitrary commands or files outside a worker owner's configured tasks. State what was actually tested when reporting completion.
