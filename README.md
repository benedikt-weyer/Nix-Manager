# Nix-Manager
Nix Graphical Manager

A backend that updates and rebuilds a NixOS flake, and GUI frontends for it.
One backend serves any number of frontend instances at the same time.

| Directory | What |
| --- | --- |
| `nix-backend-generic/` | gRPC backend over a unix socket. Owns `proto/nix_backend.proto`. |
| `nix-backend-client/` | Frontend-agnostic client library shared by every frontend. |
| `nix-frontend-iced/` | [iced](https://iced.rs) frontend with [mina](https://crates.io/crates/mina) animations and Lucide icons. |
| `nix-frontend-cli/` | Command-line frontend (`nix-manager`) with shell completions. |

## Development

```sh
direnv allow        # or: nix develop
./scripts/help      # all dev commands
./scripts/run       # build, start the backend and a frontend window
./scripts/run -n 3  # three windows on the same backend
```

Flakes only see git-tracked files: `git add flake.nix flake.lock` before using `direnv`/`nix develop`.

`./scripts/run` defaults to operating on `/etc/nixos`; use `-c DIR -H HOST` to point it at a scratch flake while developing.

## CLI

```sh
./scripts/run --cli status           # against the dev backend
nix-manager rebuild test             # against a real one
source <(nix-manager completions zsh)   # zsh tab completion (after compinit)
```

## Multiple frontends

Every frontend follows the backend's `Watch` stream, so an update or rebuild started in one window shows up live in all of them. The backend runs one update/rebuild at a time and rejects a second with `FAILED_PRECONDITION`.
