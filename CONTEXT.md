# arthron

Local-first code intelligence. Each file is parsed in isolation; a single
resolver links the references between files into a verified graph and records
what it cannot link.

## Language

**Extractor**:
The single-file layer that turns ast-grep matches into `Definition` and
`Reference` records. It is forbidden from linking.
_Avoid_: detector, analyzer, rule engine

**Definition**:
A named declaration that a reference elsewhere could name — a function, method,
type, constant, or module.

**Reference**:
A site in one file that names something possibly defined elsewhere — a call,
import, or type use. Carries kind, raw target text, scope, and span.

**Resolver**:
The only layer allowed to link. Classifies every reference into exactly one
outcome.

**Outcome**:
The result of resolving one reference: `Resolved`, `External`, or `Unresolved`
with a reason. There is no way to express "dropped".

**Node**:
A thing a reference can name — a definition, a module/package, or an external
package. Files and locals are not nodes.
_Avoid_: file node, anchor node

**Edge**:
A reference that resolved. Nothing else creates an edge.
_Avoid_: contains, defines (as edge kinds)

**Candidate**:
One fully-qualified name a reference might resolve to, in scope-priority order.
Every candidate probed — hit or miss — is recorded.

**Resolution rate**:
`Resolved / (Resolved + Unresolved)` for one language. Never aggregated across
languages. The primary quality gate.

**Tier**:
A language's declared capability level. Tier 1: definitions, references, call
graph. Tier 2: definitions and structure; no verified call edges.

**Corpus**:
A pinned, vendored snapshot of real source against which resolution rate is
measured.
