# mnemo

Long-term memory for agents, models and people.

mnemo keeps everything you send it as raw episodes, distills them into **memories that answer
what, who, when, where, why and how** over unique entities (people, teams, systems, repos, files…),
and searches them in memory without calling a model: about **8 ms at the 95th percentile over a
million memories**.

- **Distills on its own.** When a conversation goes quiet, a model proposes memories and a judge
  decides each one against what is already known: new, duplicate, update or contradiction. An update
  closes the old memory instead of deleting it, so you can always see what was true before.
- **Recalls before the model answers.** Claude Code and Codex hooks add what mnemo knows to each
  prompt; any other client can use MCP or plain HTTP.
- **Measures whether memory helps.** It checks whether each injected memory was used and stops
  injecting the ones that never are.
- **One binary, one ClickHouse.** Every memory belongs to one person; sign-in with keys or any OIDC
  provider.

Documentation: [docs/](docs/README.md) — architecture, data model, distillation, the judge, recall,
embeddings, storage, auth, clients, cost and value, operations.

## Quick start

mnemo stores everything in ClickHouse. [`compose.yaml`](compose.yaml) runs both:

```sh
export MNEMO_ADMIN_KEY=$(openssl rand -hex 24)
docker compose up -d --build        # http://127.0.0.1:7700
```

Remember something and ask for it back:

```sh
auth=(-H "Authorization: Bearer $MNEMO_ADMIN_KEY" -H "Content-Type: application/json")
curl -s localhost:7700/v1/memories "${auth[@]}" \
  -d '{"scope": "user:admin", "what": "The staging database runs PostgreSQL 17.", "kind": "fact"}'
curl -s localhost:7700/v1/recall "${auth[@]}" -d '{"query": "which postgres does staging use?"}'
```

To distill conversations, give it an OpenAI-compatible model (Ollama, vLLM, llama.cpp, OpenRouter…):

```sh
MNEMO_LLM_URL=http://host.docker.internal:11434/v1 MNEMO_EXTRACT_MODEL=qwen3:14b docker compose up -d
```

Without one, mnemo still stores everything and finds it again by text and meaning. For yourself and
each person, create a key instead of sharing the admin key:

```sh
docker compose exec mnemo mnemo key create --subject ana   # prints the key once
```

## Claude Code

```text
/plugin marketplace add https://github.com/mariojcr/mnemo.git
/plugin install mnemo@mnemo
```

The plugin:

- **remembers each turn**: your prompt and Claude's final answer go to mnemo as episodes;
- **recalls before each turn**: what mnemo knows about your prompt is added to Claude's context;
- **knows the repo**: each session starts with what is known about the repository you are in;
- **adds tools**: `recall`, `remember`, `forget` and `recall_episode`, over MCP.

It talks to the server you point it to and never starts anything on your machine. Put
`MNEMO_URL=https://mnemo.example.com` and `MNEMO_KEY=mnemo_…` in `~/.mnemo/config.env` (the
environment wins over the file). On an SSO-only server, run `mnemo login https://mnemo.example.com`
once instead of using a key; see [docs/clients.md](docs/clients.md).

The plugin uses the `mnemo` binary from your `PATH`. Each [release](https://github.com/mariojcr/mnemo/releases)
publishes a static Linux x86_64 build with `release.txt`, a manifest of its sum and its commit, tag
and build number, signed (Ed25519); the public key is `deploy/release-key.pub`. Install it once,
checking the signature and the sum:

```sh
base=https://github.com/mariojcr/mnemo/releases/latest/download
curl -fsSLO https://raw.githubusercontent.com/mariojcr/mnemo/main/deploy/release-key.pub
curl -fsSLO $base/mnemo-linux-x86_64 && curl -fsSLO $base/release.txt && curl -fsSLO $base/release.txt.sig
base64 -d release.txt.sig > release.sig.raw \
  && openssl pkeyutl -verify -pubin -inkey release-key.pub -rawin -in release.txt -sigfile release.sig.raw \
  && grep '^ref refs/tags/v' release.txt && grep '^commit' release.txt \
  && grep ' mnemo-linux-x86_64$' release.txt | sha256sum -c && install -m 755 mnemo-linux-x86_64 ~/.local/bin/mnemo
```

From then on `mnemo update` does the same in place, with the public key built in, and refuses a
build older than the one you have (`--allow-downgrade` to go back on purpose).

## Codex

The same binary and login work in Codex: register `mnemo mcp` and install the hooks in
[`integrations/codex/hooks.json`](integrations/codex/hooks.json). See
[docs/clients.md](docs/clients.md#codex).

## Other clients

**MCP**, over HTTP at `/mcp` or over stdio with `mnemo mcp`:

```json
{ "mcpServers": {
    "mnemo": { "type": "http", "url": "https://mnemo.example.com/mcp", "headers": { "Authorization": "Bearer mnemo_…" } } } }
```

**HTTP**:

| Route | What it does |
|---|---|
| `POST /v1/recall` | `{query, limit?, as_of?, w?, entities?, since?, until?, semantic?, raw?, budget_tokens?}`: ranked memories, raw excerpts if needed, and a `text` block ready for a model. |
| `POST /v1/memories` | Store a memory: `{scope, what, why?, how?, kind?, who?, where?, about?, occurred_at?}`. Returns `inserted`, `duplicate` or `superseded`. |
| `POST /v1/forget` | `{id, reason}`: close a memory. |
| `POST /v1/episodes` | `{episodes, cursor?}`: raw episodes; a source's cursor advances only if the batch was stored. |
| `GET /v1/episodes/{id}?around=` | An episode with its neighbours in the thread. |
| `POST /v1/entities`, `GET /v1/entities/{id}` | Entities, and an entity's card with its timeline. |
| `POST /v1/records`, `GET /v1/records?key=` | Records of things: events (open, close or point) and snapshots. |
| `GET /v1/globe`, `GET /v1/stats`, `GET /v1/whoami` | Everything visible, counts, and who you are. |
| `GET /v1/galaxy?z&x&y` | A tile of the map. |
| `DELETE /v1/owners/{id}` | Delete everything of a person, raw episodes included. |

The server also serves a web UI at `/`: a map of everything you know (entities as galaxies and
systems, memories as dust between them), search, and an admin panel.

## Configuration

### Models

Extraction and judging use endpoints you configure; mnemo starts no model. Embeddings are computed
inside mnemo (EmbeddingGemma 2 on ONNX Runtime, on the CPU, about 50 ms per memory); the image ships
the model.

| Variable | What it does |
|---|---|
| `MNEMO_LLM_URL`, `MNEMO_LLM_KEY` | OpenAI-compatible root (`…/v1`) for extraction. |
| `MNEMO_EXTRACT_MODEL` | The model that turns conversations into memories. Without it, nothing is distilled. |
| `MNEMO_LLM_JSON=1`, `MNEMO_LLM_THINKING=off` | Ask for schema-constrained JSON, and ask a reasoning model not to think first (some return empty JSON after a long think). |
| `MNEMO_DECIDE_URL`, `MNEMO_DECIDE_MODEL`, `MNEMO_DECIDE_KEY` | Optional typed-decisions service (`POST /decisions`) to judge with calibrated probabilities. |
| `MNEMO_JUDGE` | `auto` (decisions, else the extraction model, else a heuristic), `decisions`, `llm` or `heuristic`. |
| `MNEMO_EMBED_DIR`, `MNEMO_EMBED_THREADS`, `MNEMO_EMBED=off` | Embedding model files outside the image, CPU threads (4), or semantic search off. |

A small model works if it supports constrained JSON: on real work sessions in English and Spanish,
Gemma 4 E4B judged 8 of 8 relations right and extracted correct facts; 2B models broke the schema.

### Keys, scopes and sign-in

Every memory belongs to one person (`user:<id>`); nothing is global or shared. Every request carries a
key, a session or an OAuth token; there is no keyless mode.

| Kind | Sees and writes |
|---|---|
| `user` | its own memory |
| `service` | the personal scopes it passes: a system that already authorizes its users; with `X-Mnemo-Act-As` a request becomes that person's ([docs/auth.md](docs/auth.md#acting-for-someone)) |
| `admin` | its own memory; manages keys and people, but does not read anyone else's |

People can sign in to the UI with any OIDC provider and create their own keys (up to 90 days):

```sh
MNEMO_OIDC_ISSUER=https://sso.example.com/realms/acme
MNEMO_OIDC_CLIENT_ID=mnemo MNEMO_OIDC_CLIENT_SECRET=…
MNEMO_BASE_URL=https://mnemo.example.com          # the redirect is <base>/auth/callback
MNEMO_ADMIN_GROUPS=mnemo-admins                     # or MNEMO_ADMIN_EMAILS
```

For a server on the internet, `MNEMO_AUTH=sso` disables keys entirely, and `/mcp` becomes an OAuth
protected resource so MCP clients such as Claude sign in with your provider. See
[docs/auth.md](docs/auth.md).

### Scaling

There is one writer per database: `mnemo serve` hands out the log sequence and distills, and holds a
lease in ClickHouse so a second `serve` refuses to start. Read replicas forward writes to it:

```sh
MNEMO_CH_URL=http://clickhouse:8123 MNEMO_WRITER_URL=http://mnemo-writer:7700 mnemo api
```

### Cost and value

The **Usage** view (and `GET /v1/cost`) shows what share of session input is memory, how often
an injected memory was used, and what mnemo spends in the background. A session never receives the
same memory twice, automatic injection stops at `MNEMO_SESSION_BUDGET_TOKENS` (4000) per session, and
a memory judged useless `MNEMO_MUTE_AFTER` times (6) stops being injected. See
[docs/value.md](docs/value.md).

## How it works

1. **Ingest everything, raw.** Episodes are immutable and keep who, where and when.
2. **Distill when a thread settles.** The extraction model proposes memories with their six W's;
   the judge decides each one ([distillation](docs/distillation.md), [the judge](docs/judge.md)).
3. **Recall fast.** Each server keeps the globe in memory and ranks without a model
   ([recall](docs/recall.md)).

Memory text is stored in English, because models read it.

## Development

Everything builds and runs in a container; no Rust on the host.

```sh
make test          # fmt, clippy, unit tests and the storage suite against a throwaway ClickHouse
make bench N=1000000
make image         # runtime image
make release       # static Linux binary in dist/
```

## License

[Apache-2.0](LICENSE).
