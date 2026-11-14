# Pintail schema-migration differential gate

Measured 2026-11-14T11:31:25.366Z against `mysql:8.4`.

**403 passed, 0 failed.**

| Family | Check | Status | Detail |
|---|---|---|---|
| integer width narrows | mirrors the source before the migration | PASS |  |
| integer width widens | mirrors the source before the migration | PASS |  |
| signedness changes | mirrors the source before the migration | PASS |  |
| decimal digits shrink | mirrors the source before the migration | PASS |  |
| decimal digits grow | mirrors the source before the migration | PASS |  |
| decimal becomes double | mirrors the source before the migration | PASS |  |
| varchar capacity shrinks | mirrors the source before the migration | PASS |  |
| varchar becomes fixed-width char | mirrors the source before the migration | PASS |  |
| varbinary becomes fixed-width binary | mirrors the source before the migration | PASS |  |
| char becomes varchar | mirrors the source before the migration | PASS |  |
| varchar becomes longtext | mirrors the source before the migration | PASS |  |
| text family shrinks | mirrors the source before the migration | PASS |  |
| varchar becomes int | mirrors the source before the migration | PASS |  |
| text becomes blob | mirrors the source before the migration | PASS |  |
| latin1 becomes utf8mb4 | mirrors the source before the migration | PASS |  |
| collation becomes case sensitive | mirrors the source before the migration | PASS |  |
| datetime becomes timestamp | mirrors the source before the migration | PASS |  |
| timestamp becomes datetime | mirrors the source before the migration | PASS |  |
| datetime loses fractional seconds | mirrors the source before the migration | PASS |  |
| datetime becomes date | mirrors the source before the migration | PASS |  |
| enum members reorder | mirrors the source before the migration | PASS |  |
| enum member is dropped | mirrors the source before the migration | PASS |  |
| enum member is renamed | mirrors the source before the migration | PASS |  |
| enum member is appended | mirrors the source before the migration | PASS |  |
| set members reorder | mirrors the source before the migration | PASS |  |
| set member is appended | mirrors the source before the migration | PASS |  |
| bit width narrows | mirrors the source before the migration | PASS |  |
| nullable becomes not null | mirrors the source before the migration | PASS |  |
| not null becomes nullable | mirrors the source before the migration | PASS |  |
| varbinary capacity shrinks | mirrors the source before the migration | PASS |  |
| integer width narrows | the source rewrote the row it already held | PASS |  |
| integer width widens | the source left the row it already held alone | PASS |  |
| signedness changes | the source rewrote the row it already held | PASS |  |
| decimal digits shrink | the source rewrote the row it already held | PASS |  |
| decimal digits grow | the source left the row it already held alone | PASS |  |
| decimal becomes double | the source rewrote the row it already held | PASS |  |
| varchar capacity shrinks | the source rewrote the row it already held | PASS |  |
| varchar becomes fixed-width char | the source rewrote the row it already held | PASS |  |
| varbinary becomes fixed-width binary | the source rewrote the row it already held | PASS |  |
| char becomes varchar | the source left the row it already held alone | PASS |  |
| varchar becomes longtext | the source left the row it already held alone | PASS |  |
| text family shrinks | the source rewrote the row it already held | PASS |  |
| varchar becomes int | the source rewrote the row it already held | PASS |  |
| text becomes blob | the source left the row it already held alone | PASS |  |
| latin1 becomes utf8mb4 | the source left the row it already held alone | PASS |  |
| collation becomes case sensitive | the source left the row it already held alone | PASS |  |
| datetime becomes timestamp | the source rewrote the row it already held | PASS |  |
| timestamp becomes datetime | the source left the row it already held alone | PASS |  |
| datetime loses fractional seconds | the source rewrote the row it already held | PASS |  |
| datetime becomes date | the source rewrote the row it already held | PASS |  |
| enum members reorder | the source left the row it already held alone | PASS |  |
| enum member is dropped | the source rewrote the row it already held | PASS |  |
| enum member is renamed | the source rewrote the row it already held | PASS |  |
| enum member is appended | the source left the row it already held alone | PASS |  |
| set members reorder | the source rewrote the row it already held | PASS |  |
| set member is appended | the source left the row it already held alone | PASS |  |
| bit width narrows | the source rewrote the row it already held | PASS |  |
| nullable becomes not null | the source rewrote the row it already held | PASS |  |
| not null becomes nullable | the source left the row it already held alone | PASS |  |
| varbinary capacity shrinks | the source rewrote the row it already held | PASS |  |
| stored generated expression changes | the source recomputed the row it already held | PASS |  |
| virtual generated expression changes | the source recomputed the row it already held | PASS |  |
| integer width narrows | the whole table matches the source after the migration | PASS |  |
| integer width narrows | rows the migration never wrote to are not stale | PASS |  |
| integer width widens | the whole table matches the source after the migration | PASS |  |
| integer width widens | rows the migration never wrote to are not stale | PASS |  |
| signedness changes | the whole table matches the source after the migration | PASS |  |
| signedness changes | rows the migration never wrote to are not stale | PASS |  |
| decimal digits shrink | the whole table matches the source after the migration | PASS |  |
| decimal digits shrink | rows the migration never wrote to are not stale | PASS |  |
| decimal digits grow | the whole table matches the source after the migration | PASS |  |
| decimal digits grow | rows the migration never wrote to are not stale | PASS |  |
| decimal becomes double | the whole table matches the source after the migration | PASS |  |
| decimal becomes double | rows the migration never wrote to are not stale | PASS |  |
| varchar capacity shrinks | the whole table matches the source after the migration | PASS |  |
| varchar capacity shrinks | rows the migration never wrote to are not stale | PASS |  |
| varchar becomes fixed-width char | the whole table matches the source after the migration | PASS |  |
| varchar becomes fixed-width char | rows the migration never wrote to are not stale | PASS |  |
| varbinary becomes fixed-width binary | the whole table matches the source after the migration | PASS |  |
| varbinary becomes fixed-width binary | rows the migration never wrote to are not stale | PASS |  |
| char becomes varchar | the whole table matches the source after the migration | PASS |  |
| char becomes varchar | rows the migration never wrote to are not stale | PASS |  |
| varchar becomes longtext | the whole table matches the source after the migration | PASS |  |
| varchar becomes longtext | rows the migration never wrote to are not stale | PASS |  |
| text family shrinks | the whole table matches the source after the migration | PASS |  |
| text family shrinks | rows the migration never wrote to are not stale | PASS |  |
| varchar becomes int | the whole table matches the source after the migration | PASS |  |
| varchar becomes int | rows the migration never wrote to are not stale | PASS |  |
| text becomes blob | the whole table matches the source after the migration | PASS |  |
| text becomes blob | rows the migration never wrote to are not stale | PASS |  |
| latin1 becomes utf8mb4 | the whole table matches the source after the migration | PASS |  |
| latin1 becomes utf8mb4 | rows the migration never wrote to are not stale | PASS |  |
| collation becomes case sensitive | the whole table matches the source after the migration | PASS |  |
| collation becomes case sensitive | rows the migration never wrote to are not stale | PASS |  |
| datetime becomes timestamp | the whole table matches the source after the migration | PASS |  |
| datetime becomes timestamp | rows the migration never wrote to are not stale | PASS |  |
| timestamp becomes datetime | the whole table matches the source after the migration | PASS |  |
| timestamp becomes datetime | rows the migration never wrote to are not stale | PASS |  |
| datetime loses fractional seconds | the whole table matches the source after the migration | PASS |  |
| datetime loses fractional seconds | rows the migration never wrote to are not stale | PASS |  |
| datetime becomes date | the whole table matches the source after the migration | PASS |  |
| datetime becomes date | rows the migration never wrote to are not stale | PASS |  |
| enum members reorder | the whole table matches the source after the migration | PASS |  |
| enum members reorder | rows the migration never wrote to are not stale | PASS |  |
| enum member is dropped | the whole table matches the source after the migration | PASS |  |
| enum member is dropped | rows the migration never wrote to are not stale | PASS |  |
| enum member is renamed | the whole table matches the source after the migration | PASS |  |
| enum member is renamed | rows the migration never wrote to are not stale | PASS |  |
| enum member is appended | the whole table matches the source after the migration | PASS |  |
| enum member is appended | rows the migration never wrote to are not stale | PASS |  |
| set members reorder | the whole table matches the source after the migration | PASS |  |
| set members reorder | rows the migration never wrote to are not stale | PASS |  |
| set member is appended | the whole table matches the source after the migration | PASS |  |
| set member is appended | rows the migration never wrote to are not stale | PASS |  |
| bit width narrows | the whole table matches the source after the migration | PASS |  |
| bit width narrows | rows the migration never wrote to are not stale | PASS |  |
| nullable becomes not null | the whole table matches the source after the migration | PASS |  |
| nullable becomes not null | rows the migration never wrote to are not stale | PASS |  |
| not null becomes nullable | the whole table matches the source after the migration | PASS |  |
| not null becomes nullable | rows the migration never wrote to are not stale | PASS |  |
| varbinary capacity shrinks | the whole table matches the source after the migration | PASS |  |
| varbinary capacity shrinks | rows the migration never wrote to are not stale | PASS |  |
| stored generated expression changes | the whole table matches the source after the migration | PASS |  |
| stored generated expression changes | rows the migration never wrote to are not stale | PASS |  |
| virtual generated expression changes | the whole table matches the source after the migration | PASS |  |
| virtual generated expression changes | rows the migration never wrote to are not stale | PASS |  |
| text defaults placed first and after a column | the whole table matches the source after the migration | PASS |  |
| text defaults placed first and after a column | rows the migration never wrote to are not stale | PASS |  |
| a latin1 text default | the whole table matches the source after the migration | PASS |  |
| a latin1 text default | rows the migration never wrote to are not stale | PASS |  |
| enum and set defaults | the whole table matches the source after the migration | PASS |  |
| enum and set defaults | rows the migration never wrote to are not stale | PASS |  |
| numeric defaults | the whole table matches the source after the migration | PASS |  |
| numeric defaults | rows the migration never wrote to are not stale | PASS |  |
| a float default with more digits than the catalogue prints | the whole table matches the source after the migration | PASS |  |
| a float default with more digits than the catalogue prints | rows the migration never wrote to are not stale | PASS |  |
| bit and binary defaults | the whole table matches the source after the migration | PASS |  |
| bit and binary defaults | rows the migration never wrote to are not stale | PASS |  |
| calendar and clock literal defaults | the whole table matches the source after the migration | PASS |  |
| calendar and clock literal defaults | rows the migration never wrote to are not stale | PASS |  |
| zero-date defaults and implicit zero dates | the whole table matches the source after the migration | PASS |  |
| zero-date defaults and implicit zero dates | rows the migration never wrote to are not stale | PASS |  |
| timestamp literal defaults in a daylight-saving zone | the whole table matches the source after the migration | PASS |  |
| timestamp literal defaults in a daylight-saving zone | rows the migration never wrote to are not stale | PASS |  |
| current timestamp defaults in UTC | the whole table matches the source after the migration | PASS |  |
| current timestamp defaults in UTC | rows the migration never wrote to are not stale | PASS |  |
| current timestamp defaults in a daylight-saving zone | the whole table matches the source after the migration | PASS |  |
| current timestamp defaults in a daylight-saving zone | rows the migration never wrote to are not stale | PASS |  |
| a column added and dropped again | the whole table matches the source after the migration | PASS |  |
| a column added and dropped again | rows the migration never wrote to are not stale | PASS |  |
| a column added and given a new default | the whole table matches the source after the migration | PASS |  |
| a column added and given a new default | rows the migration never wrote to are not stale | PASS |  |
| columns added in turn, first and after | the whole table matches the source after the migration | PASS |  |
| columns added in turn, first and after | rows the migration never wrote to are not stale | PASS |  |
| an expression default | the whole table matches the source after the migration | PASS |  |
| an expression default | rows the migration never wrote to are not stale | PASS |  |
| a json expression default | the whole table matches the source after the migration | PASS |  |
| a json expression default | rows the migration never wrote to are not stale | PASS |  |
| a stored generated column | the whole table matches the source after the migration | PASS |  |
| a stored generated column | rows the migration never wrote to are not stale | PASS |  |
| a virtual generated column | the whole table matches the source after the migration | PASS |  |
| a virtual generated column | rows the migration never wrote to are not stale | PASS |  |
| a statement led by a comment | the whole table matches the source after the migration | PASS |  |
| a statement led by a comment | rows the migration never wrote to are not stale | PASS |  |
| a comment between the opening words | the whole table matches the source after the migration | PASS |  |
| a comment between the opening words | rows the migration never wrote to are not stale | PASS |  |
| a line break between the opening words | the whole table matches the source after the migration | PASS |  |
| a line break between the opening words | rows the migration never wrote to are not stale | PASS |  |
| two text columns added back to back | the whole table matches the source after the migration | PASS |  |
| two text columns added back to back | rows the migration never wrote to are not stale | PASS |  |
| a column and its index in one statement | the whole table matches the source after the migration | PASS |  |
| a column and its index in one statement | rows the migration never wrote to are not stale | PASS |  |
| a column and a unique key over it in one statement | the whole table matches the source after the migration | PASS |  |
| a column and a unique key over it in one statement | rows the migration never wrote to are not stale | PASS |  |
| a nullable column beside ALGORITHM | the whole table matches the source after the migration | PASS |  |
| a nullable column beside ALGORITHM | rows the migration never wrote to are not stale | PASS |  |
| a text column placed after another beside ALGORITHM | the whole table matches the source after the migration | PASS |  |
| a text column placed after another beside ALGORITHM | rows the migration never wrote to are not stale | PASS |  |
| not null over a column holding no null | the whole table matches the source after the migration | PASS |  |
| not null over a column holding no null | rows the migration never wrote to are not stale | PASS |  |
| integer narrows around values that fit | the whole table matches the source after the migration | PASS |  |
| integer narrows around values that fit | rows the migration never wrote to are not stale | PASS |  |
| integer narrows around a row the stream carried just before | the whole table matches the source after the migration | PASS |  |
| integer narrows around a row the stream carried just before | rows the migration never wrote to are not stale | PASS |  |
| varchar shrinks around values that fit | the whole table matches the source after the migration | PASS |  |
| varchar shrinks around values that fit | rows the migration never wrote to are not stale | PASS |  |
| varchar shrinks around a value one character too long | the whole table matches the source after the migration | PASS |  |
| varchar shrinks around a value one character too long | rows the migration never wrote to are not stale | PASS |  |
| text family shrinks around values that fit | the whole table matches the source after the migration | PASS |  |
| text family shrinks around values that fit | rows the migration never wrote to are not stale | PASS |  |
| varbinary shrinks around values that fit | the whole table matches the source after the migration | PASS |  |
| varbinary shrinks around values that fit | rows the migration never wrote to are not stale | PASS |  |
| enum drops a member no row holds | the whole table matches the source after the migration | PASS |  |
| enum drops a member no row holds | rows the migration never wrote to are not stale | PASS |  |
| two narrowings in one statement, one of them over a null | the whole table matches the source after the migration | PASS |  |
| two narrowings in one statement, one of them over a null | rows the migration never wrote to are not stale | PASS |  |
| varchar shrinks and widens again with a long row after | the whole table matches the source after the migration | PASS |  |
| varchar shrinks and widens again with a long row after | rows the migration never wrote to are not stale | PASS |  |
| varchar widens beside ALGORITHM and LOCK | the whole table matches the source after the migration | PASS |  |
| varchar widens beside ALGORITHM and LOCK | rows the migration never wrote to are not stale | PASS |  |
| not null relaxed beside ALGORITHM and LOCK | the whole table matches the source after the migration | PASS |  |
| not null relaxed beside ALGORITHM and LOCK | rows the migration never wrote to are not stale | PASS |  |
| integer narrows around values that fit beside ALGORITHM | the whole table matches the source after the migration | PASS |  |
| integer narrows around values that fit beside ALGORITHM | rows the migration never wrote to are not stale | PASS |  |
| enum gains a member beside ALGORITHM | the whole table matches the source after the migration | PASS |  |
| enum gains a member beside ALGORITHM | rows the migration never wrote to are not stale | PASS |  |
| column renamed beside ALGORITHM | the whole table matches the source after the migration | PASS |  |
| column renamed beside ALGORITHM | rows the migration never wrote to are not stale | PASS |  |
| unique key added beside ALGORITHM and LOCK | the whole table matches the source after the migration | PASS |  |
| unique key added beside ALGORITHM and LOCK | rows the migration never wrote to are not stale | PASS |  |
| column dropped with its index | the whole table matches the source after the migration | PASS |  |
| column dropped with its index | rows the migration never wrote to are not stale | PASS |  |
| index renamed | the whole table matches the source after the migration | PASS |  |
| index renamed | rows the migration never wrote to are not stale | PASS |  |
| index dropped | the whole table matches the source after the migration | PASS |  |
| index dropped | rows the migration never wrote to are not stale | PASS |  |
| one-column index made composite | the whole table matches the source after the migration | PASS |  |
| one-column index made composite | rows the migration never wrote to are not stale | PASS |  |
| unique key made composite beside the primary key | the whole table matches the source after the migration | PASS |  |
| unique key made composite beside the primary key | rows the migration never wrote to are not stale | PASS |  |
| primary key made composite | the whole table matches the source after the migration | PASS |  |
| primary key made composite | rows the migration never wrote to are not stale | PASS |  |
| an alter with no clause | the whole table matches the source after the migration | PASS |  |
| an alter with no clause | rows the migration never wrote to are not stale | PASS |  |
| a text column added, widened and renamed while the mirror was down | the whole table matches the source after the migration | PASS |  |
| a text column added, widened and renamed while the mirror was down | rows the migration never wrote to are not stale | PASS |  |
| a column added, dropped and added again under another type while the mirror was down | the whole table matches the source after the migration | PASS |  |
| a column added, dropped and added again under another type while the mirror was down | rows the migration never wrote to are not stale | PASS |  |
| two columns added to a latin1 table and one dropped while the mirror was down | the whole table matches the source after the migration | PASS |  |
| two columns added to a latin1 table and one dropped while the mirror was down | rows the migration never wrote to are not stale | PASS |  |
| a rebuilt copy swapped in under the table name | the whole table matches the source after the migration | PASS |  |
| a rebuilt copy swapped in under the table name | rows the migration never wrote to are not stale | PASS |  |
| columns added and widened in the middle of a stream of writes | the whole table matches the source after the migration | PASS |  |
| columns added and widened in the middle of a stream of writes | rows the migration never wrote to are not stale | PASS |  |
| integer width narrows | taken by recopying the table | PASS |  |
| integer width widens | taken without a quarantine | PASS |  |
| signedness changes | taken by recopying the table | PASS |  |
| decimal digits shrink | taken by recopying the table | PASS |  |
| decimal digits grow | taken without a quarantine | PASS |  |
| decimal becomes double | taken by recopying the table | PASS |  |
| varchar capacity shrinks | taken by recopying the table | PASS |  |
| varchar becomes fixed-width char | taken by recopying the table | PASS |  |
| varbinary becomes fixed-width binary | taken by recopying the table | PASS |  |
| char becomes varchar | taken without a quarantine | PASS |  |
| varchar becomes longtext | taken without a quarantine | PASS |  |
| text family shrinks | taken by recopying the table | PASS |  |
| varchar becomes int | taken by recopying the table | PASS |  |
| text becomes blob | taken by recopying the table | PASS |  |
| latin1 becomes utf8mb4 | taken without a quarantine | PASS |  |
| collation becomes case sensitive | taken without a quarantine | PASS |  |
| datetime becomes timestamp | taken by recopying the table | PASS |  |
| timestamp becomes datetime | taken by recopying the table | PASS |  |
| datetime loses fractional seconds | taken by recopying the table | PASS |  |
| datetime becomes date | taken by recopying the table | PASS |  |
| enum members reorder | taken without a quarantine | PASS |  |
| enum member is dropped | taken by recopying the table | PASS |  |
| enum member is renamed | taken by recopying the table | PASS |  |
| enum member is appended | taken without a quarantine | PASS |  |
| set members reorder | taken by recopying the table | PASS |  |
| set member is appended | taken without a quarantine | PASS |  |
| bit width narrows | taken by recopying the table | PASS |  |
| nullable becomes not null | taken by recopying the table | PASS |  |
| not null becomes nullable | taken without a quarantine | PASS |  |
| varbinary capacity shrinks | taken by recopying the table | PASS |  |
| text defaults placed first and after a column | evolved in place, never quarantined | PASS |  |
| a latin1 text default | evolved in place, never quarantined | PASS |  |
| enum and set defaults | evolved in place, never quarantined | PASS |  |
| numeric defaults | evolved in place, never quarantined | PASS |  |
| a float default with more digits than the catalogue prints | evolved in place, never quarantined | PASS |  |
| bit and binary defaults | evolved in place, never quarantined | PASS |  |
| calendar and clock literal defaults | evolved in place, never quarantined | PASS |  |
| zero-date defaults and implicit zero dates | evolved in place, never quarantined | PASS |  |
| timestamp literal defaults in a daylight-saving zone | evolved in place, never quarantined | PASS |  |
| current timestamp defaults in UTC | evolved in place, never quarantined | PASS |  |
| current timestamp defaults in a daylight-saving zone | evolved in place, never quarantined | PASS |  |
| a column added and dropped again | evolved in place, never quarantined | PASS |  |
| a column added and given a new default | evolved in place, never quarantined | PASS |  |
| columns added in turn, first and after | evolved in place, never quarantined | PASS |  |
| an expression default | recopied, with the reason given | PASS |  |
| a json expression default | recopied, with the reason given | PASS |  |
| a stored generated column | recopied, with the reason given | PASS |  |
| a virtual generated column | recopied, with the reason given | PASS |  |
| a statement led by a comment | evolved in place, never quarantined | PASS |  |
| a comment between the opening words | evolved in place, never quarantined | PASS |  |
| a line break between the opening words | evolved in place, never quarantined | PASS |  |
| two text columns added back to back | evolved in place, never quarantined | PASS |  |
| a column and its index in one statement | evolved in place, never quarantined | PASS |  |
| a column and a unique key over it in one statement | recopied, with the reason given | PASS |  |
| a nullable column beside ALGORITHM | evolved in place, never quarantined | PASS |  |
| a text column placed after another beside ALGORITHM | evolved in place, never quarantined | PASS |  |
| not null over a column holding no null | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| integer narrows around values that fit | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| integer narrows around a row the stream carried just before | recopied, with the reason given | PASS |  |
| varchar shrinks around values that fit | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| varchar shrinks around a value one character too long | recopied, with the reason given | PASS |  |
| text family shrinks around values that fit | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| varbinary shrinks around values that fit | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| enum drops a member no row holds | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| two narrowings in one statement, one of them over a null | recopied, with the reason given | PASS |  |
| varchar widens beside ALGORITHM and LOCK | taken without a quarantine | PASS |  |
| not null relaxed beside ALGORITHM and LOCK | taken without a quarantine | PASS |  |
| integer narrows around values that fit beside ALGORITHM | adopted in place on the evidence of its rows, never quarantined | PASS |  |
| enum gains a member beside ALGORITHM | taken without a quarantine | PASS |  |
| column renamed beside ALGORITHM | taken without a quarantine | PASS |  |
| unique key added beside ALGORITHM and LOCK | taken without a quarantine | PASS |  |
| column dropped with its index | recopied, with the reason given | PASS |  |
| index renamed | taken without a quarantine | PASS |  |
| index dropped | taken without a quarantine | PASS |  |
| one-column index made composite | taken without a quarantine | PASS |  |
| unique key made composite beside the primary key | taken without a quarantine | PASS |  |
| primary key made composite | recopied, with the reason given | PASS |  |
| an alter with no clause | taken without a quarantine | PASS |  |
| a text column added, widened and renamed while the mirror was down | taken by recopying the table | PASS |  |
| a column added, dropped and added again under another type while the mirror was down | taken without a quarantine | PASS |  |
| two columns added to a latin1 table and one dropped while the mirror was down | taken without a quarantine | PASS |  |
| a rebuilt copy swapped in under the table name | taken without a quarantine | PASS |  |
| columns added and widened in the middle of a stream of writes | taken without a quarantine | PASS |  |
| integer width narrows | the table still matches the source after a restart | PASS |  |
| integer width widens | the table still matches the source after a restart | PASS |  |
| signedness changes | the table still matches the source after a restart | PASS |  |
| decimal digits shrink | the table still matches the source after a restart | PASS |  |
| decimal digits grow | the table still matches the source after a restart | PASS |  |
| decimal becomes double | the table still matches the source after a restart | PASS |  |
| varchar capacity shrinks | the table still matches the source after a restart | PASS |  |
| varchar becomes fixed-width char | the table still matches the source after a restart | PASS |  |
| varbinary becomes fixed-width binary | the table still matches the source after a restart | PASS |  |
| char becomes varchar | the table still matches the source after a restart | PASS |  |
| varchar becomes longtext | the table still matches the source after a restart | PASS |  |
| text family shrinks | the table still matches the source after a restart | PASS |  |
| varchar becomes int | the table still matches the source after a restart | PASS |  |
| text becomes blob | the table still matches the source after a restart | PASS |  |
| latin1 becomes utf8mb4 | the table still matches the source after a restart | PASS |  |
| collation becomes case sensitive | the table still matches the source after a restart | PASS |  |
| datetime becomes timestamp | the table still matches the source after a restart | PASS |  |
| timestamp becomes datetime | the table still matches the source after a restart | PASS |  |
| datetime loses fractional seconds | the table still matches the source after a restart | PASS |  |
| datetime becomes date | the table still matches the source after a restart | PASS |  |
| enum members reorder | the table still matches the source after a restart | PASS |  |
| enum member is dropped | the table still matches the source after a restart | PASS |  |
| enum member is renamed | the table still matches the source after a restart | PASS |  |
| enum member is appended | the table still matches the source after a restart | PASS |  |
| set members reorder | the table still matches the source after a restart | PASS |  |
| set member is appended | the table still matches the source after a restart | PASS |  |
| bit width narrows | the table still matches the source after a restart | PASS |  |
| nullable becomes not null | the table still matches the source after a restart | PASS |  |
| not null becomes nullable | the table still matches the source after a restart | PASS |  |
| varbinary capacity shrinks | the table still matches the source after a restart | PASS |  |
| stored generated expression changes | the table still matches the source after a restart | PASS |  |
| virtual generated expression changes | the table still matches the source after a restart | PASS |  |
| text defaults placed first and after a column | the table still matches the source after a restart | PASS |  |
| a latin1 text default | the table still matches the source after a restart | PASS |  |
| enum and set defaults | the table still matches the source after a restart | PASS |  |
| numeric defaults | the table still matches the source after a restart | PASS |  |
| a float default with more digits than the catalogue prints | the table still matches the source after a restart | PASS |  |
| bit and binary defaults | the table still matches the source after a restart | PASS |  |
| calendar and clock literal defaults | the table still matches the source after a restart | PASS |  |
| zero-date defaults and implicit zero dates | the table still matches the source after a restart | PASS |  |
| timestamp literal defaults in a daylight-saving zone | the table still matches the source after a restart | PASS |  |
| current timestamp defaults in UTC | the table still matches the source after a restart | PASS |  |
| current timestamp defaults in a daylight-saving zone | the table still matches the source after a restart | PASS |  |
| a column added and dropped again | the table still matches the source after a restart | PASS |  |
| a column added and given a new default | the table still matches the source after a restart | PASS |  |
| columns added in turn, first and after | the table still matches the source after a restart | PASS |  |
| an expression default | the table still matches the source after a restart | PASS |  |
| a json expression default | the table still matches the source after a restart | PASS |  |
| a stored generated column | the table still matches the source after a restart | PASS |  |
| a virtual generated column | the table still matches the source after a restart | PASS |  |
| a statement led by a comment | the table still matches the source after a restart | PASS |  |
| a comment between the opening words | the table still matches the source after a restart | PASS |  |
| a line break between the opening words | the table still matches the source after a restart | PASS |  |
| two text columns added back to back | the table still matches the source after a restart | PASS |  |
| a column and its index in one statement | the table still matches the source after a restart | PASS |  |
| a column and a unique key over it in one statement | the table still matches the source after a restart | PASS |  |
| a nullable column beside ALGORITHM | the table still matches the source after a restart | PASS |  |
| a text column placed after another beside ALGORITHM | the table still matches the source after a restart | PASS |  |
| not null over a column holding no null | the table still matches the source after a restart | PASS |  |
| integer narrows around values that fit | the table still matches the source after a restart | PASS |  |
| integer narrows around a row the stream carried just before | the table still matches the source after a restart | PASS |  |
| varchar shrinks around values that fit | the table still matches the source after a restart | PASS |  |
| varchar shrinks around a value one character too long | the table still matches the source after a restart | PASS |  |
| text family shrinks around values that fit | the table still matches the source after a restart | PASS |  |
| varbinary shrinks around values that fit | the table still matches the source after a restart | PASS |  |
| enum drops a member no row holds | the table still matches the source after a restart | PASS |  |
| two narrowings in one statement, one of them over a null | the table still matches the source after a restart | PASS |  |
| varchar shrinks and widens again with a long row after | the table still matches the source after a restart | PASS |  |
| varchar widens beside ALGORITHM and LOCK | the table still matches the source after a restart | PASS |  |
| not null relaxed beside ALGORITHM and LOCK | the table still matches the source after a restart | PASS |  |
| integer narrows around values that fit beside ALGORITHM | the table still matches the source after a restart | PASS |  |
| enum gains a member beside ALGORITHM | the table still matches the source after a restart | PASS |  |
| column renamed beside ALGORITHM | the table still matches the source after a restart | PASS |  |
| unique key added beside ALGORITHM and LOCK | the table still matches the source after a restart | PASS |  |
| column dropped with its index | the table still matches the source after a restart | PASS |  |
| index renamed | the table still matches the source after a restart | PASS |  |
| index dropped | the table still matches the source after a restart | PASS |  |
| one-column index made composite | the table still matches the source after a restart | PASS |  |
| unique key made composite beside the primary key | the table still matches the source after a restart | PASS |  |
| primary key made composite | the table still matches the source after a restart | PASS |  |
| an alter with no clause | the table still matches the source after a restart | PASS |  |
| a text column added, widened and renamed while the mirror was down | the table still matches the source after a restart | PASS |  |
| a column added, dropped and added again under another type while the mirror was down | the table still matches the source after a restart | PASS |  |
| two columns added to a latin1 table and one dropped while the mirror was down | the table still matches the source after a restart | PASS |  |
| a rebuilt copy swapped in under the table name | the table still matches the source after a restart | PASS |  |
| columns added and widened in the middle of a stream of writes | the table still matches the source after a restart | PASS |  |
