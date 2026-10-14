# Distillation

From raw episodes to memories, in the background, on the writer only.

```mermaid
flowchart TB
    IN["POST /v1/episodes<br/>(hooks, any client)"] --> RAW[("episodes")]
    IN --> TH[("threads<br/>last activity per thread")]
    TH -- "quiet for MNEMO_SETTLE_SECONDS (120)" --> READY{"extraction model,<br/>embedder and judge<br/>all answer?"}
    READY -- no --> PAUSE["pause: 30 s, doubling to 5 min<br/>(nothing is claimed)"]
    PAUSE --> READY
    READY -- yes --> CLAIM["claim up to 4 threads<br/>lease 15 min"]
    CLAIM --> PASSES["up to 60 pending episodes per thread,<br/>split into passes"]
    PASSES --> X1["extract pass 1"] & X2["extract pass 2"] & X3["… up to MNEMO_DISTILL_PARALLEL (4)"]
    X1 & X2 & X3 --> SETTLE["settle passes in order"]
    SETTLE --> DONE["advance the thread cursor"]
```

## A pass

```mermaid
sequenceDiagram
    participant D as distill loop
    participant X as extraction model
    participant E as embedder
    participant J as judge
    participant DB as ClickHouse

    D->>X: system prompt + known entities + earlier context + new episodes
    X-->>D: JSON {entities, memories} (schema-constrained with MNEMO_LLM_JSON=1)
    D->>D: validate: drop memories without a supporting episode<br/>and links to unknown entities, merge same-pass duplicates,<br/>add entities named in the text
    D->>E: embed each candidate (what + why + how)
    D->>D: pick the source passage for each candidate
    par for each candidate
        D->>J: candidate + up to 8 neighbours + passage
        J-->>D: verdict
    end
    D->>DB: write plan (inserts, duplicates, closes, new entities)<br/>only if nothing changed on those entities since the judgement
    alt something changed meanwhile
        D->>D: refresh and judge again (3 attempts in all)
    end
```

## How much goes into a pass

```mermaid
flowchart LR
    T["thread with N new episodes"] --> B{"fits the pass budget?<br/>MNEMO_EXTRACT_BUDGET_TOKENS (6,000) × 3 chars"}
    B -- yes --> ONE["one pass"]
    B -- no --> MANY["several passes, each with the last<br/>6 episodes of the previous one as context"]
    ONE & MANY --> EP["each episode enters whole<br/>(up to its 6,000-char chunk)"]
    EP --> OUT{"output cut by max_tokens?"}
    OUT -- "yes, several episodes" --> HALF["half the episodes, again"]
    OUT -- "yes, one episode" --> CUT["read half as much of it, with a warning<br/>(under 500 chars, skip it with a warning)"]
    OUT -- no --> OK["settle"]
```

The pass budget is also capped by what fits in `MNEMO_LLM_CONTEXT` (16,384 tokens) next to the
system prompt and the output, which is capped at min(3,000, context/4) tokens. Each call times out
after `MNEMO_LLM_TIMEOUT` (600 s).

The cursor only advances over what was actually settled, so a failure never leaves a gap.

## What the extraction prompt asks for

- Only what helps a **future, different** conversation: facts, decisions and their reasons,
  causes and fixes, procedures, preferences. No chit-chat, no progress snapshots ("tests are
  green", "52 memories").
- `why` and `how` only when the transcript says them. Never invented.
- `who` only for people, teams or named bots.
- Each line says who speaks, and only a person's own lines carry their name: `user <name>`,
  `assistant`, `tool` (a background task or a subagent report; a `<task-notification>` sent as a
  user prompt is shown as `tool`). Clients stamp the person on the whole session, so the name is
  kept off the lines that are not theirs.
- A **proposal is not a decision** until a person accepts it or it is applied. What an assistant
  or a tool line concludes is a fact or a state, never a person's decision. This is also enforced
  after the model: a `decision` that no line of a person supports becomes a `fact`, and loses its
  `decided_by` people.
- **Dropping** a plan ("not worth it", "let's leave it") is stored as a decision that names the
  plan, so the earlier plan can be closed.
- No **work log**: a pushed commit, a deploy or an edited file is not a memory unless it leaves a
  behaviour, a cause, a procedure or a constraint that later work needs.
- A fix is written as its own event naming the same thing, so the old problem can be closed.
- Reuse known entities, also when the transcript names one in another language. New entities get
  English names, with the transcript's term as an alias.
- Text in English.

## Failure behaviour

| Situation | What happens |
|---|---|
| A backend is down | Nothing is claimed; the loop waits (30 s, doubling to 5 min) and logs which backend |
| A pass fails | Passes before it stay settled; the thread is retried, at the latest when its 15-minute lease ends (no retry storms) |
| A write conflicts | The pass refreshes and judges again, 3 attempts in all |
| Output overflows on one episode | It is read in halves, with a warning; under 500 characters it is skipped with a warning |
