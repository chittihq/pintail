# Pintail schema-migration differential gate

Measured 2026-09-12T14:45:41.202Z against `mysql:8.4`.

**143 passed, 0 failed.**

| Family | Check | Status | Detail |
|---|---|---|---|
| integer width narrows | mirrors the source before the migration | PASS |  |
| integer width widens | mirrors the source before the migration | PASS |  |
| signedness changes | mirrors the source before the migration | PASS |  |
| decimal digits shrink | mirrors the source before the migration | PASS |  |
| decimal digits grow | mirrors the source before the migration | PASS |  |
| decimal becomes double | mirrors the source before the migration | PASS |  |
| varchar capacity shrinks | mirrors the source before the migration | PASS |  |
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
| integer width narrows | the table still matches the source after a restart | PASS |  |
| integer width widens | the table still matches the source after a restart | PASS |  |
| signedness changes | the table still matches the source after a restart | PASS |  |
| decimal digits shrink | the table still matches the source after a restart | PASS |  |
| decimal digits grow | the table still matches the source after a restart | PASS |  |
| decimal becomes double | the table still matches the source after a restart | PASS |  |
| varchar capacity shrinks | the table still matches the source after a restart | PASS |  |
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
