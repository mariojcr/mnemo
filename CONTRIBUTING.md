<!--
SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
SPDX-License-Identifier: Apache-2.0
-->

# Contributing to mnemo

Documentation fixes, bug reports, client integrations and reproducible examples are welcome.
For a substantial behavior or architecture change, open an issue first to discuss the use case.

## Report a problem

Use the [issue templates](https://github.com/mariojcr/mnemo/issues/new/choose). Include the release
or commit, platform, client integration, reproduction steps and expected versus actual behavior.
Remove keys, conversation content and personal or internal infrastructure details from logs.
For vulnerabilities, follow [SECURITY.md](SECURITY.md).

## Develop and check a change

Git, Docker, Make and OpenSSL are required. Rust builds and tests run in containers; no host Rust installation
is needed. Read the [architecture](docs/architecture.md) and the documentation for the component you
are changing.

```sh
make test             # formatting, clippy, unit and ClickHouse storage tests
make image            # builds the local runtime image
```

`make test` creates or reuses the `mnemo-test-clickhouse` container on local port `18123`. You can
remove that test container when finished with `docker rm -f mnemo-test-clickhouse`.
For HTTP changes, include a reproduction against a disposable server with the appropriate
credentials and personal scopes.

Keep pull requests focused. Describe the user-visible change and the checks you ran; update the
relevant docs and include a regression test when fixing behavior. For documentation-only changes,
check links and commands instead of running unrelated test suites. Use fictional data in examples
and screenshots. By contributing, you agree that your contributions use the project's Apache-2.0
license.

## File licensing

Keep the SPDX copyright and Apache-2.0 identifier in comment headers on project-owned source,
configuration and documentation files. Use the comment syntax for the file type, keeping shebangs
and Docker parser directives first. `REUSE.toml` covers generated files and formats that cannot
carry comments. Bundled fonts retain their upstream OFL-1.1 license and attribution.

When adding files, check coverage with `reuse lint` (the optional [REUSE tool](https://reuse.software/)).
License texts are in `LICENSES/`; the root `LICENSE` remains the project's Apache-2.0 license.
