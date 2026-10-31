# Nix-Manager
Nix Graphical Manager

A backend that updates and rebuilds a NixOS flake, and GUI frontends for it.
One backend serves any number of frontend instances at the same time.

| Directory | What |
| --- | --- |
| `nix-backend-generic/` | gRPC backend over a unix socket. Owns `proto/nix_backend.proto`. |
| `nix-backend-client/` | Frontend-agnostic client library shared by every frontend. |
| `nix-agent-client/` | Frontend-agnostic [Agent Client Protocol](https://agentclientprotocol.com) client: runs an AI coding agent on the config directory. |
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

## Install (NixOS)

The flake builds everything with [crane](https://github.com/ipetkov/crane):

```sh
nix build .#default            # backend + CLI + GUI together
nix run .#nix-manager-gui      # or .#nix-manager-cli, .#nix-backend-generic
```

As a NixOS module (adds the programs and a systemd user service for the backend):

```nix
inputs.nix-manager.url = "github:benedikt-weyer/Nix-Manager";
# ...
modules = [ inputs.nix-manager.nixosModules.default ];
# configuration.nix:
programs.nix-manager.enable = true;   # programs.nix-manager.backend.enable = false to skip the service
```

The installed programs use their own socket (`$XDG_RUNTIME_DIR/nix-manager.sock`), so they don't collide with
another `nix-backend-generic` (e.g. the shell's). The GUI ships with a logo (`assets/icons/nix-manager.svg`)
and a desktop entry.

## CLI

```sh
./scripts/run --cli status           # against the dev backend
nix-manager rebuild test             # against a real one
source <(nix-manager completions zsh)   # zsh tab completion (after compinit)
```

## Assistant

The sparkles button opens a chat with an AI agent that can explain the configuration or change it. The agent is any program that speaks the [Agent Client Protocol](https://agentclientprotocol.com) on its standard input and output; set the command in Settings (default `claude-code-acp`; others include `codex-acp` and `gemini --experimental-acp`).

It runs as you, in the config directory, and starts when the page is first opened. It asks before it edits files or runs commands, and nothing is ever allowed on your behalf. Its edits land in the files like any other change, so review and commit them on the git page. It is told not to rebuild or commit itself.

## Multiple frontends

Every frontend follows the backend's `Watch` stream, so an update or rebuild started in one window shows up live in all of them. The backend runs one update/rebuild at a time and rejects a second with `FAILED_PRECONDITION`.
