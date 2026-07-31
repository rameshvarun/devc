# devc

[![GitHub Release](https://img.shields.io/github/v/release/rameshvarun/devc)](https://github.com/rameshvarun/devc/releases/latest)

`devc` is an alternate [devcontainers](https://containers.dev/) CLI with a simplified interface. It's based on the [upstream reference CLI](https://github.com/devcontainers/cli), but ported to Rust by Claude and packaged as a single self-contained binary.

Currently macOS and Linux are supported. As a prerequisite, you need to have a Docker-compatible container runtime installed.

## Quickstart

The CLI can be installed in a few ways.

```bash
# Install the CLI using Homebrew
brew install rameshvarun/tap/devc

# Or, use the install script, which downloads to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/rameshvarun/devc/main/install.sh | sh

# Or, download it manually from the releases page
# https://github.com/rameshvarun/devc/releases/latest
```

Once the CLI is installed, switch to a project that has a `.devcontainer` spec.

```bash
cd project/ # Switch to your project's directory.
devc npm test # Run any command inside the container, starting it if it doesn't exist.
devc # Or, run with no arguments to get a shell inside the container.
```

## Additional Usage

```bash
devc --help # Print usage information
```

## Motivation

Containers make it easy to create reproducible, sandboxed dev environments - that's why the [devcontainers](https://containers.dev/) specification exists. Although well integrated into VSCode, devcontainers aren't as convenient to use from the command line. `devc` bridges that gap by providing a simplified interface for launching and using devcontainers.

```bash
# Using the reference CLI
devcontainer up
devcontainer exec npm test

# Using devc
devc npm test
```
