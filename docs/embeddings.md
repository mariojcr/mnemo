# Embeddings

mnemo computes every vector itself, with **EmbeddingGemma 2** in fp32 on ONNX Runtime, on the
CPU. There is no embedding service and no other embedding model.

```mermaid
flowchart LR
    subgraph image["container image"]
        ORT["libonnxruntime 1.28.3<br/>sha256-pinned"]
        MOD["EmbeddingGemma 2 fp32<br/>pinned revision, sha256-pinned<br/>1.1 GB"]
    end
    subgraph process["mnemo process"]
        CHK["check size and sha256<br/>(again every minute until they match)"] --> LOAD["load session<br/>(MNEMO_EMBED_THREADS, 4)"]
        LOAD --> SEM["one inference at a time"]
    end
    MOD --> CHK
    ORT --> LOAD
    SEM --> Q["embed_query (recall)"]
    SEM --> D["embed (new memories)"]
    SEM --> P["embed_source (judge passages)"]
    SEM --> N["embed_entity (name merging)"]
```

- The image carries the model. mnemo **never downloads** it: it reads `MNEMO_EMBED_DIR` (outside
  the image, `$MNEMO_HOME/models/embeddinggemma-2`) and the library at `ORT_DYLIB_PATH`. While the
  files are missing or do not match, the embedder reports why and distillation waits.
- Prompts: `task: search result | query: ` for questions, `title: none | text: ` for what is stored.
- 768 dimensions, normalized. The globe keeps them quantized to int8.
- `MNEMO_EMBED=off` runs without vectors: no search by meaning, judge passages picked by word
  overlap, no merging of translated entity names.

## Why fp32

Measured on an 8-core laptop CPU (i7-11800H, 4 threads), warm, median of 5:

| Build | 16 tokens | 53 tokens (a typical memory) | 132 tokens | 2,048 tokens | RAM loaded |
|---|---|---|---|---|---|
| **fp32** | 29 ms | **50 ms** | 111 ms | 4.1 s | 684 MB |
| int8 | 82 ms | 113 ms | 178 ms | 4.3 s | 457 MB |
| q4 | 24 ms | 52 ms | 127 ms | 4.4 s | 404 MB |

The int8 build uses dynamic quantization: it quantizes activations on every call, which costs
more than it saves on short texts. Vectors barely change between fp32 and int8 (mean cosine
0.9999); q4 drifts more (0.985). Going from 4 to 8 threads gains about 10 %.

## Limits

Cost grows with the square of the length:

```mermaid
xychart-beta
    title "Extra peak memory by text length"
    x-axis "tokens per text" [1024, 2048, 4096, 8192]
    y-axis "extra MB" 0 --> 6000
    bar [120, 500, 1700, 5900]
```

| Setting | Default | Why |
|---|---|---|
| `MNEMO_EMBED_MAX_TOKENS` | 2,048 (longer texts are truncated) | a native 8K text needs ~6 GB and 14 s; real memories are at most ~200 tokens (median 53) |
| batch size | ≤ 8,192 positions per call | a huge query or many long texts cannot exhaust memory |
| query length | 2,000 characters | a search never needs more |
| concurrency | one inference at a time | waiting work is abandoned at its deadline, not queued |

## When the model changes

```mermaid
flowchart LR
    START["writer: at start,<br/>then every minute"] --> FP{"fingerprint of the model name<br/>= cursor embed_model?"}
    FP -- yes --> FILL["only fill live memories without a vector"]
    FP -- no --> ALL["recalculate every vector,<br/>closed memories included"]
    ALL --> SAVE["save the new fingerprint"]
```

Vectors from different models are not comparable (two deployments of the same model gave cosine
−0.02 for the same text), and a foreign vector silently hides its memory from search. Admin →
Status → *Recalculate all vectors* (`POST /v1/admin/reembed`) forces a full recalculation.

## Metering

Every inference is recorded in `llm_calls` with kind `embed`, `embed_query`, `embed_source` or
`embed_entity`, its token count and how long the inference took (not the wait for a turn).
