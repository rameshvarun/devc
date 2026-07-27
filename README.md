# devc

`devc` is a [devcontainers](https://containers.dev/) CLI with a simplified interface. It's based off the [upstream reference CLI](https://github.com/devcontainers/cli), but rewritten in Rust by Claude and packaged as a single self contained binary.

```bash
# Install the script from the Homebrew tap.
brew install rameshvarun/tap/devc
# Or, install the CLI via the script, or get it from the releases page.
curl -fsSL https://raw.githubusercontent.com/rameshvarun/devc/main/install.sh | sh

cd project/ # Swap to your project's directory.
devc # This will start the container if it doesn't exist, dropping you into a shell.
```

## Additional Usage

```bash
devc --help # Print usage information
devc cat /etc/os-release # Run any command inside the container.
```
