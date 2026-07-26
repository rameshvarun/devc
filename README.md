# devc

`devc` is a [devcontainers](https://containers.dev/) CLI with a simplified interface. It's based off the [upstream reference CLI](https://github.com/devcontainers/cli), but rewritten in Rust by Claude.

```bash
# Install the CLI using the script, or get it from the releases page.
curl -fsSL https://raw.githubusercontent.com/rameshvarun/devc/main/install.sh | sh

cd project/ # cd into your project's directory.
devc # This will start the container if it doesn't exist, and open up a shell.
```