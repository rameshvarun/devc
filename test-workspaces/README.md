# test-workspaces

Sample workspaces for exercising `devc`. `cd` into any of these and run `devc` (opens a shell) or
`devc <command>...` (runs the command inside the container, e.g. `devc ./gradlew check`).

- `root-devcontainer/` — a root-level `.devcontainer.json` (no `.devcontainer/` folder).
- `image-simple/` — image-based container with `containerEnv`, `remoteEnv`, variable substitution,
  a bind-mounted file (`hello.txt`), and a `postCreateCommand`.
- `dockerfile-build/` — container built from a `Dockerfile` with a build arg; `postCreateCommand`
  as an argv array.
- `compose-unsupported/` — uses `dockerComposeFile`; devc should error clearly (unsupported).
- `features-java/` — uses Dev Container `features` (Java + GitHub CLI from ghcr.io); devc fetches the
  OCI features, builds an extended image, and installs them. Exercised by the `#[ignore]`d
  `features_install_java_and_gh` test (slow; needs network — run with `cargo test -- --ignored`).
- `no-config/` — no dev container config at all; devc should error explaining none was found.

These workspaces are also driven by the black-box tests in `tests/e2e.rs` (`cargo test`).
