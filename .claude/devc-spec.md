# `devc` Spec
devc is a cross-platform Rust CLI (MacOS, Linux) that serves as an alternative to devcontainers-cli with a simpler interface.

All a user has to do is `cd` into a directory with a .devcontainer spec and run `devc` no args. This will build and start up the container if it isn't already running, and it will start a shell inside the container.

```shell
devc # (No args), opens a shell
devc ./gradlew check # Directly pass a command to run. Starts the container if it isn't already running.
```

The devcontainer reference CLI is provided under ./devcontainers-cli/ for reference.

## Testing Plan

### Testing Workspaces

The repo will have a folder ./test-workspaces with folders containing workspaces with simple devcontainer configurations. For example you may have a file like ./test-workspaces/root-devcontainer/.devcontainer.json

### E2E CLI Tests

Tests will be written under tests/ and designed to be run with `cargo test`. These tests will use the CLI as a black box executable. These tests will for example, run CLI commands in the test-workspaces and inspect the output.