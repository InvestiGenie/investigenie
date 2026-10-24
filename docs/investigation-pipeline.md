# AI Investigation Pipeline

The investigation planner receives Data View metadata, not a dump of source records. A bounded evidence engine validates the plan, runs datasource queries, and sends only compact samples and aggregations to hypothesis generation. Connection credentials stay on the server.

```mermaid
flowchart TD
    Q[Question and selected Data Views] --> M[Metadata collection<br/>field names and types]
    M --> P[Evidence plan<br/>filters, sample fields, aggregation field]
    P --> V[Validate plan against metadata]
    V --> E[Evidence gathering engine]
    E -->|bounded sample query| S[Small set of source records]
    E -->|aggregate query| A[Time histogram and optional top values]
    S --> H[Hypothesis engine<br/>explain evidence and propose causes]
    A --> H
    H --> R[Findings with cited sample IDs]
    R --> UI[Investigation result]
    U[Manual Knowledge management] --> K[Encrypted knowledge entries<br/>service context, runbooks, resolved incidents]
    R -->|Save as knowledge draft| K
    H -. future automatic proposals .-> K
    K -. future hints .-> P
```

## Boundaries

- Planning uses Data View names, indices, field names, and field types. It returns candidate filters, a bounded list of sample fields, and an optional aggregation field.
- The evidence engine validates those choices and sends one bounded search request per selected Data View for at most four records plus a time histogram. Adapters may make additional database-side aggregation/preflight requests. AI Elasticsearch requests disable exact hit counting. The result can include one top-value breakdown (up to three values plus `__other__` per bucket) when supported. Sample records include at most four selected fields, values are capped at 256 bytes, and the hypothesis payload keeps at most 16 histogram buckets. It does not page through every matching record.
- Hypothesis generation receives the question, validated scope, aggregate summaries, and compact sample evidence. Findings may cite only IDs included in those samples.
- Manual knowledge storage and management are implemented, with draft/published/archived status and revision-checked edits. Completed live/history results can be explicitly copied into unverified drafts with their investigation reference and evidence scope, then reviewed in the Knowledge editor. Publishing is an editorial decision, not proof of correctness. Automatic entry creation, retrieval, and investigation citation snapshots remain future work; investigations currently run without knowledge entries.

## Future Knowledge and Investigation Behavior

The knowledge base is intended to make investigations more repeatable, not to replace current evidence:

- Build on manually curated service context, runbooks, and resolved incidents. Preserve source/scope context and distinguish publication from verification. Future citations must retain the entry title, revision, and excerpt used even after an entry changes or is deleted.
- Retrieve entries as candidate guidance for planning. A match may suggest filters, queries, or a known-case workflow, but each investigation must recheck the current connected data and cite evidence from that run. Never present a stored solution as a current finding by itself.
- Route known patterns through deterministic queries and decision steps where possible, including a visible baseline and evidence gates. Keep the bounded hypothesis path for unknown cases, and allow an explicit insufficient-evidence result rather than forcing a cause.
- Distinguish observed evidence from inferred causal links and proposed explanations; retain relevant blind spots and missing telemetry in the result.
- Record outcomes such as confirmed resolutions, dismissed hypotheses, false positives, and proposed actions as reviewable history. Feedback may suggest a KB or rule revision, but must not silently change verified knowledge. Promote a solution only after an explicit verification signal; validate detection changes against both benign and malicious examples.
- Evaluate deterministic and model-assisted paths on the same regression cases. Compare evidence validity and investigation quality alongside latency and cost, per task, rather than relying on an unverified headline speed claim.
- Defer continuous streaming ingestion and an external MCP interface until a concrete workflow requires them. MCP exposure additionally requires authentication and per-caller authorization.
