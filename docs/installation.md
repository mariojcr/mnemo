<!--
SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
SPDX-License-Identifier: Apache-2.0
-->

# Installation

## Supported platforms

Published server images and standalone clients currently target **Linux x86_64 (amd64)**. Native
macOS, Windows and ARM64 clients are not published. Container emulation on other platforms has not
been validated; do not assume that installing the Claude Code plugin also installs a native client.
An HTTP MCP client can connect to a supported server without the standalone binary.

The server needs ClickHouse. The image includes ONNX Runtime and an embedding model (~1.3 GB for
the image); RAM needs grow with the in-memory index. An extraction model runs separately and has
its own resource requirements. There is no measured universal minimum RAM recommendation yet.

## Start the server

Follow the [quick start](../README.md#quick-start). Compose uses the published `0.1.1` image and
binds the API/UI to loopback. It is a local evaluation configuration: use your own credentials,
TLS and access controls for an internet-facing deployment. See [operations](operations.md).

Keep `MNEMO_ADMIN_KEY` available when running Compose again. To persist it locally, put it in a
`.env` file with permissions `600`; `.env` is ignored by Git. Never commit credentials.

To choose another published version:

```sh
MNEMO_IMAGE=ghcr.io/mariojcr/mnemo:0.1.1 docker compose up -d
```

To run a local development build:

```sh
make image
MNEMO_IMAGE=mnemo:local docker compose up -d
```

`docker compose down` preserves volumes. Adding `--volumes` deletes this Compose project's stored
data. If startup fails, inspect `docker compose logs mnemo clickhouse` before retrying.

## Install the client

The client is needed for the Claude Code hooks and the `mnemo mcp` stdio bridge. On Linux x86_64,
with curl, OpenSSL, coreutils and a POSIX shell, run this from your clone. The public key comes from
the repository you are inspecting; downloaded release files are checked before installation.

```sh
(
  set -eu
  base=https://github.com/mariojcr/mnemo/releases/latest/download
  public_key="$PWD/deploy/release-key.pub"
  test -r "$public_key"
  download_dir=$(mktemp -d)
  trap 'rm -rf "$download_dir"' EXIT
  cd "$download_dir"
  curl -fsSLO "$base/mnemo-linux-x86_64"
  curl -fsSLO "$base/release.txt"
  curl -fsSLO "$base/release.txt.sig"
  base64 -d release.txt.sig > release.sig.raw
  openssl pkeyutl -verify -pubin -inkey "$public_key" -rawin \
    -in release.txt -sigfile release.sig.raw
  grep '^ref refs/tags/v' release.txt
  grep '^commit ' release.txt
  grep ' mnemo-linux-x86_64$' release.txt > binary.sha256
  sha256sum -c binary.sha256
  mkdir -p "$HOME/.local/bin"
  install -m 755 mnemo-linux-x86_64 "$HOME/.local/bin/mnemo"
)
export PATH="$HOME/.local/bin:$PATH"
mnemo version
```

Add the `PATH` line to your shell's startup file if needed. Later, `mnemo update` verifies the
signed manifest and checksum and replaces the binary in place. It refuses an older build unless
you pass `--allow-downgrade`.

Create `~/.mnemo/config.env` with permissions `600`:

```text
MNEMO_URL=http://127.0.0.1:7700
MNEMO_KEY=mnemo_REPLACE_WITH_YOUR_PERSONAL_KEY
```

Create the directory first (`mkdir -p ~/.mnemo`). Use the personal key created in the quick start;
the placeholder above is not a working key. Environment variables override the file. For OIDC,
use `mnemo login https://mnemo.example.com` instead; see [authentication](auth.md#the-command-line).

Then follow the [client integration guide](clients.md).
