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
    H -. future .-> K[Knowledge base<br/>issue pattern, verified solution, approach]
    K -. future hints .-> P
```

## Boundaries

- Planning uses Data View names, indices, field names, and field types. It returns candidate filters, a bounded list of sample fields, and an optional aggregation field.
- The evidence engine validates those choices and sends one bounded search request per selected Data View for at most four records plus a time histogram. Adapters may make additional database-side aggregation/preflight requests. AI Elasticsearch requests disable exact hit counting. The result can include one top-value breakdown (up to three values plus `__other__` per bucket) when supported. Sample records include at most four selected fields, values are capped at 256 bytes, and the hypothesis payload keeps at most 16 histogram buckets. It does not page through every matching record.
- Hypothesis generation receives the question, validated scope, aggregate summaries, and compact sample evidence. Findings may cite only IDs included in those samples.
- Knowledge-base storage and retrieval are future work. The intended record is the issue pattern, its verified solution, and the approach that found it; nothing is persisted by this stage yet.
