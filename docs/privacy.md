<!--
SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
SPDX-License-Identifier: Apache-2.0
-->

# Data flow and deletion

mnemo is self-hosted. Its deployment configuration determines where data is stored and which model
providers receive content.

| Operation | Data involved | Destination |
|---|---|---|
| Claude Code / Codex hooks | Prompts, final answers, session metadata and available token usage; repository identity at session start | Your mnemo server |
| Storage | Raw episodes, memories, entities, revisions and usage records | Your ClickHouse deployment |
| Embeddings | Memory text and search queries | ONNX Runtime inside the mnemo process, on the CPU |
| Distillation and judging | Conversation passages, proposed memories and relevant existing memories | Your configured extraction / judge endpoints |
| Usage evaluation | Injected memories and the completed answer | Your configured judge, when evaluation requires it |
| Recall | Selected memories and, when needed, source excerpts | The requesting client, which may pass them to its model provider |
| OIDC sign-in | Identity claims and authentication exchanges | Your configured identity provider and mnemo server |

The hooks read supported local transcript fields to report token usage. They do not upload the
entire transcript file as a file attachment. Web content is saved explicitly with `keep`; the
client does not automatically capture every fetched page. See [clients](clients.md).

Prompts and answers can contain credentials or confidential text. Enable capture only for sessions
you intend to retain, and choose model endpoints appropriate for that content. Running embeddings
locally does not make distillation local: configure a local LLM endpoint if that is required.

## Access

Memory scopes belong to individual people. A normal key only accesses its owner's memory. Admins
manage users, keys and numeric usage reports; their normal memory queries still use their own scope.
Service keys are privileged and can act across personal scopes. In key mode, an admin can create a
key for another person, so deployment administrators remain trusted. [Full access model](auth.md).

API credentials authenticate requests; they are not end-to-end encryption. The deployment operator
controls ClickHouse, backups, logs and infrastructure access.

## Forgetting and deletion

`forget` closes a memory so it is no longer current. It preserves the history and source episodes;
explicit recall can still retrieve historical memories.

`DELETE /v1/owners/{id}` deletes the person's data, including raw episodes, from mnemo's store.
It requires an authorized credential. Deleting a person's data does not erase copies already held
by model providers, client transcripts, logs or infrastructure backups; those follow their own
retention policies. See [HTTP API](clients.md#http-api) and [storage](storage.md).
