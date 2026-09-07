# Why these ten workloads

Selected from the current engine and its documented constraints, not from the
old settled-only benchmark ranking. The source used for measurements includes
the overlapping-pair compaction fix; that limitation is not treated as still open.
These are workload families, not a claim to cover all MySQL compatibility gaps.

| Workload | Real update scenario | Engine question this screen investigates |
|---|---|---|
| Scan/filter/project | An update changes whether a row matches or what it projects | Does better predicate execution still matter after visibility resolution and decode? |
| Latest versions/tombstones | Repeated updates and deletes leave overlapping versions | Which merge representation reduces read work, including construction, without reviving old rows? |
| Low-cardinality grouping | Rows move between a small number of groups | Can dense/local aggregation pay for rebuilding after changes? |
| High-cardinality grouping/top-K | Updated amounts reorder many groups | Can grouping and top-K avoid complete ordered materialization? |
| Join/group aggregation | Changes on either input alter membership and duplicate multiplicity | Does a different build/probe or factorized aggregate improve the complete query? |
| Exact grouped distinct | Inserts/deletes and NULL changes alter exact distinct membership | Which exact representation works across skew and changing group membership? |
| Ordered LIMIT | A changed ordering value moves a row into or out of the result | Can selection avoid full sorting while preserving deterministic ties and NULL order? |
| Bounded rolling windows | Earlier values change the output of later frames | Can sliding state remove repeated frame scans while retaining exact SUM/COUNT/MIN? |
| IN/NOT IN | Changes introduce or remove NULL from the membership set | Can faster probing retain three-valued outcomes after each rebuild? |
| Correlated aggregates | Outer and inner keys change; many outer rows repeat a key | Can demand filtering, memoization or decorrelation reduce repeated inner work? |

[The executable alternatives](APPROACHES.md) cover ten mechanisms in each family.
The full query engine already implements some of these mechanisms; a lab reference
is a simple correctness/control implementation, not a model of the current planner.
A lab ratio cannot establish that replacing an existing engine mechanism would help.

The current [limitations](../../docs/limitations.md) still motivate further engine
work beyond this screen: expensive bounded frames, dependent-plan rebuilding,
skewed spill paths, untracked memory, compaction admission/debt and freshness delay.
Compatibility limitations involving types, collation or unsupported SQL require
separate correctness work; this integer-fixture experiment does not close them.
