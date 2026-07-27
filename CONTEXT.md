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

**LocalBinding**:
The reason carried by a reference that names a local — a parameter, local
variable, or receiver. Locals are not nodes, so this reason is caused by
policy, not by a resolver gap; it is reported beside `External` and excluded
from both terms of the resolution rate. A reason rather than a fourth
`Outcome` variant: the three-variant contract is not widened.
_Avoid_: unresolved local

**Node**:
A thing a reference can name — a definition, a module/package, or an external
package. Files and locals are not nodes.
_Avoid_: file node, anchor node

**Domain**:
The identity space a node's id is hashed in. Languages that share one
reference space share a domain — JavaScript and TypeScript are one domain, so
a `.ts` import can name a `.js` definition.
_Avoid_: language (as an identity input)

**Edge**:
A reference that resolved. Nothing else creates an edge.
_Avoid_: contains, defines (as edge kinds)

**Candidate**:
One index key a reference might resolve through, probed in scope-priority
order. A key may name a node directly or stand for one under another shape
(an alias, an overload set). Every probe — hit or miss — is recorded.

**Alias**:
An index key that names a node under another name — an export alias or a
re-export. An alias is an entry pointing at a node, not a node itself.

**Track**:
One language family's pluggable unit: its language(s), extractor, resolver,
and go-live switch, registered in a fixed order. A disabled track owns no
files. Going live edits only the track's own module.
_Avoid_: plugin, backend

**Resolution rate**:
`Resolved / (Resolved + Unresolved)` for one language. Never aggregated across
languages. The primary quality gate.

**Tier**:
A language's declared capability level. Tier 1: definitions, references, call
graph. Tier 2: definitions, structure, and imports; no verified call edges —
its gate is an import-resolution rate.

**Framework edge**:
An edge a framework rule adds, provenanced by its framework and counted per
framework. Framework coverage never moves a language's resolution rate.
_Avoid_: magic edge, implicit edge

**Framework node**:
A node a framework rule mints in its own namespace: a route, a property key,
a settings key. Counted only in that framework's tally.
_Avoid_: virtual node, synthetic node

**Framework site**:
One place a framework rule fired. The unit the framework coverage rate is
computed over. A site emitting several edges is one site.
_Avoid_: framework reference

**Policy reason**:
A framework outcome no rule improvement can change, because arthron does not
execute code or touch the network. Reported beside the rate, excluded from
both terms, and gated on drift — the same treatment `LocalBinding` gets one
layer down.
_Avoid_: skipped, ignored

**Corpus**:
A pinned, vendored snapshot of real source against which resolution rate is
measured.
