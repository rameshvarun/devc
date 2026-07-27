# devc

`devc` is an alternate [devcontainers](https://containers.dev/) CLI with a simplified interface. It's based off the [upstream reference CLI](https://github.com/devcontainers/cli), but ported to Rust by Claude and packaged as a single self contained binary.

## Quickstart

The CLI can be a installed in a few ways.

```bash
# Install the CLI using Homebrew
brew install rameshvarun/tap/devc
# Or, use the install script, which downloads to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/rameshvarun/devc/main/install.sh | sh
# Or, download it manually from the releases page
# https://github.com/rameshvarun/devc/releases/latest
```

Once the CLI is installed, swap to a project that has a `.devcontainer` spec.

```bash
cd project/ # Swap to your project's directory.
devc npm test # Run any command inside the container, starting it if it doesn't exist.
devc # Run with no arguments to get a shell inside the container.
```

## Additional Usage

```bash
devc --help # Print usage information
```
