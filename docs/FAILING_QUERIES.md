# Failing queries

The typed differential run of the 1,714-case corpus reports **58 failing cases**.
All queries below were accepted by MySQL. This is an inventory of failing
queries, not a count of independent engine defects. Some cases exercise
documented compatibility boundaries; none is counted as a pass.

Corpus implementation: `9de6b17`. The run was made before that slice was
committed; the query/fixture contents match that slice. Engine code was unchanged.
MySQL version: `8.4.11`.
Image: `mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb`.

Comparison preserves SQL NULL and binary bytes and permits float tolerance
only when both sides have approximate types. Full outcomes are generated at
`validate-out/oracle-outcomes.json`; they are not replaced with passing evidence
when failures exist. Earlier text-only comparisons could hide NULL/type errors.

| Family | Failing cases |
|---|---:|
| boundary conversion contexts | 6 |
| boundary json identity | 1 |
| boundary quantified subqueries | 17 |
| boundary result transport | 2 |
| boundary string grouping | 1 |
| boundary string storage | 12 |
| boundary temporal precision | 8 |
| boundary time durations | 4 |
| decimal conditional aggregates | 1 |
| diversify coalesce meta length | 1 |
| diversify json null rows | 1 |
| enum relational interactions | 2 |
| null truth tables | 1 |
| string boundary composition | 1 |

## 1. diversify json null rows

Case ID: `0af6cbf3aeffff8bbd03db70a9a0120029115d5498c98c98b8c9984f79c3f4a9`

```sql
SELECT id, meta IS NULL, COALESCE(JSON_LENGTH(meta), -1) FROM orders ORDER BY id;
```

MySQL returned 13 rows; Pintail returned 13 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "0"}, {"Exact": "3"}]
Pintail: [{"Exact": "1"}, {"Exact": "0"}, {"Float": "3"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 2. diversify coalesce meta length

Case ID: `2d54918186b8df293ae9637dbcb37bd12b376f859e03aebbdd4431d80ee2d33b`

```sql
SELECT id, COALESCE(JSON_LENGTH(meta, '$.tags'), 0) AS tag_n FROM orders ORDER BY id;
```

MySQL returned 13 rows; Pintail returned 13 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "1"}]
Pintail: [{"Exact": "1"}, {"Float": "1"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 3. null truth tables

Case ID: `2a6b83bebf9af23844be3255b87bb41ae810ec052f14dd091027f01553f35602`

```sql
SELECT id, note IN (SELECT note FROM events WHERE id < 0), note NOT IN (SELECT note FROM events WHERE id < 0) FROM events ORDER BY id;
```

MySQL returned 10 rows; Pintail returned 10 rows.

First differing row in the recorded output (zero-based index 2):

```text
MySQL:  [{"Exact": "3"}, {"Exact": "0"}, {"Exact": "1"}]
Pintail: [{"Exact": "3"}, "Null", "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 4. decimal conditional aggregates

Case ID: `ac5b98dd54947e594d94c9fddb9511a88b447abb8b30b680dae3597d6ade0fdb`

```sql
SELECT id, total BETWEEN 0.01 AND 50.00, total IN (0.00, 0.01, 50.00, NULL) FROM orders ORDER BY id;
```

MySQL returned 13 rows; Pintail returned 13 rows.

First differing row in the recorded output (zero-based index 1):

```text
MySQL:  [{"Exact": "2"}, {"Exact": "0"}, "Null"]
Pintail: [{"Exact": "2"}, {"Exact": "1"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 5. enum relational interactions

Case ID: `edccbfb9060acc4b63665e0bf1c7f642de9464aa0ae7a650068ba42ef439229b`

```sql
SELECT id, status = 'shipped', status = 3, status IN ('pending', 'delivered', NULL) FROM orders ORDER BY id;
```

MySQL returned 13 rows; Pintail returned 13 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "1"}, {"Exact": "1"}, "Null"]
Pintail: [{"Exact": "1"}, {"Exact": "1"}, {"Exact": "0"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 6. enum relational interactions

Case ID: `79c89ab0d29439bc0a556a1de147ea8e31542a09034dfd1cf9e16aaace5955f1`

```sql
SELECT id, CAST(status AS CHAR), CAST(status AS UNSIGNED), status + 0 FROM orders ORDER BY id;
```

MySQL returned 13 rows; Pintail returned 13 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "shipped"}, {"Exact": "3"}, {"Float": "3"}]
Pintail: [{"Exact": "1"}, {"Exact": "shipped"}, {"Exact": "0"}, {"Float": "0"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 7. string boundary composition

Case ID: `2cf73ccddf3779fd6661f2e6b138ba8ded52c38506adf350e3721713935c478e`

```sql
SELECT SUBSTRING('abc', -9), SUBSTRING('abc', 9), LPAD('abc', 5, ''), RPAD('abc', 0, 'x');
```

MySQL returned 1 rows; Pintail returned 1 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": ""}, {"Exact": ""}, {"Exact": ""}, {"Exact": ""}]
Pintail: [{"Exact": "abc"}, {"Exact": ""}, "Null", {"Exact": ""}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 8. boundary conversion contexts

Case ID: `b4a25238261a15c1c00fd259a22f7f9cb5dd0062bda182d451bb6db5d1071df7`

```sql
SELECT id, COALESCE(u, 0) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "0"}]
Pintail: [{"Exact": "1"}, {"Float": "0"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 9. boundary conversion contexts

Case ID: `a0666cac57d11076eb8045d2578ee2309fa9f6f84b95dadf826c3e11d18092fe`

```sql
SELECT id, CASE WHEN id % 2 = 0 THEN u ELSE 0 END FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "0"}]
Pintail: [{"Exact": "1"}, {"Float": "0"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 10. boundary conversion contexts

Case ID: `85c35db1f80a0576a8a3162ac74f7195ed289f2a28ae730ba16a0623ddbe61f3`

```sql
SELECT id, whole = frac FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: numeric expression overflow
```

## 11. boundary conversion contexts

Case ID: `be968435d9f86cdf309be0433b1fd14bba36634b90ca3d8317a29e7b33e042fc`

```sql
SELECT id, CASE WHEN id % 2 = 0 THEN frac ELSE 0 END FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "0"}]
Pintail: [{"Exact": "1"}, {"Exact": "0.000"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 12. boundary conversion contexts

Case ID: `5530fbba24a478cd27939a400e5c4b3a3d6b1d10cc957fef5c006c79fecd35e9`

```sql
SELECT id, CAST(approx AS CHAR) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 6):

```text
MySQL:  [{"Exact": "7"}, {"Exact": "0"}]
Pintail: [{"Exact": "7"}, {"Exact": "-0"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 13. boundary conversion contexts

Case ID: `ef454f7c46a26eb7c70ff09edc072eeeb83bafa7a5af0b0eb19d4e3eb7cfbc4c`

```sql
SELECT id, txt = frac FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 2):

```text
MySQL:  [{"Exact": "3"}, {"Exact": "1"}]
Pintail: [{"Exact": "3"}, {"Exact": "0"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 14. boundary quantified subqueries

Case ID: `a6399eda6b5ee5e80185d9655eb7e7d8d7c7ddec7ba8ba46351b72ae596cb7db`

```sql
SELECT id, n IN (SELECT n FROM bounds WHERE id < 0) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 7):

```text
MySQL:  [{"Exact": "8"}, {"Exact": "0"}]
Pintail: [{"Exact": "8"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 15. boundary quantified subqueries

Case ID: `532df501fe3432d8cee9b9f85c0189a5ffca4c0b516f11bdc93ed87ebe8b8964`

```sql
SELECT id, n NOT IN (SELECT n FROM bounds WHERE id < 0) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 7):

```text
MySQL:  [{"Exact": "8"}, {"Exact": "1"}]
Pintail: [{"Exact": "8"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 16. boundary quantified subqueries

Case ID: `e3e653b8ab622f42458f6a7040fd879f4a724ca60bc9d3a20e7ab79e7d58f07e`

```sql
SELECT id, n = ANY (SELECT n FROM bounds WHERE id < 0) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n = ANY(SELECT n FROM bounds WHERE id < 0)
```

## 17. boundary quantified subqueries

Case ID: `0ea6c412796954bb447a7ecfbb3c0dd697d6f8f6b5c71a57fd533e4d2abfadc9`

```sql
SELECT id, n > ALL (SELECT n FROM bounds WHERE id < 0) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n > ALL(SELECT n FROM bounds WHERE id < 0)
```

## 18. boundary quantified subqueries

Case ID: `6823441e6f524873d18cc1df8dec2c4c72b41c473dc2354790ed165a4ec4905b`

```sql
SELECT id, n <> ALL (SELECT n FROM bounds WHERE id < 0) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n <> ALL(SELECT n FROM bounds WHERE id < 0)
```

## 19. boundary quantified subqueries

Case ID: `bc33e42cb70b9f6792c5e9df72da3bb8a94fc0ab99a29210779d78d3e084338e`

```sql
SELECT id, n = ANY (SELECT n FROM bounds WHERE id = 8) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n = ANY(SELECT n FROM bounds WHERE id = 8)
```

## 20. boundary quantified subqueries

Case ID: `deedb9666eae4051e129f4ddf3e79447ff21a74eb50dffa015b95a64b0c71113`

```sql
SELECT id, n > ALL (SELECT n FROM bounds WHERE id = 8) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n > ALL(SELECT n FROM bounds WHERE id = 8)
```

## 21. boundary quantified subqueries

Case ID: `c8065034a64bb2610a73e47ea932f97c7a5df10a8fdf8df5ad4f01a27f15a034`

```sql
SELECT id, n <> ALL (SELECT n FROM bounds WHERE id = 8) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n <> ALL(SELECT n FROM bounds WHERE id = 8)
```

## 22. boundary quantified subqueries

Case ID: `621b3e9eeaefa14ebd61eac76f667d7f2bf07dfd779f69de7cc8bc92e04d6b6e`

```sql
SELECT id, n = ANY (SELECT n FROM bounds WHERE id = 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n = ANY(SELECT n FROM bounds WHERE id = 5)
```

## 23. boundary quantified subqueries

Case ID: `0bd1e4c8dd3b79848e91f02e435af807e1f7513dad45f999d87089fdb5096695`

```sql
SELECT id, n > ALL (SELECT n FROM bounds WHERE id = 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n > ALL(SELECT n FROM bounds WHERE id = 5)
```

## 24. boundary quantified subqueries

Case ID: `83f868920c268cff84f5ba7327e50dd3f1ddd87f5c5e2951975b56de97c0a23b`

```sql
SELECT id, n <> ALL (SELECT n FROM bounds WHERE id = 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n <> ALL(SELECT n FROM bounds WHERE id = 5)
```

## 25. boundary quantified subqueries

Case ID: `660d5d737805113fa98bb2a831fbfc1f69dbda2bba68ad6b830772db14ba0539`

```sql
SELECT id, n = ANY (SELECT n FROM bounds WHERE id >= 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n = ANY(SELECT n FROM bounds WHERE id >= 5)
```

## 26. boundary quantified subqueries

Case ID: `1a93852217e47b4c722667f80da666d87fb4ae80e086981302d39e7bcdf02a2b`

```sql
SELECT id, n > ALL (SELECT n FROM bounds WHERE id >= 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n > ALL(SELECT n FROM bounds WHERE id >= 5)
```

## 27. boundary quantified subqueries

Case ID: `c834ccb264beefce3e0010d8b0ec4b6aadcf5ce6256db7760d275105293788c7`

```sql
SELECT id, n <> ALL (SELECT n FROM bounds WHERE id >= 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n <> ALL(SELECT n FROM bounds WHERE id >= 5)
```

## 28. boundary quantified subqueries

Case ID: `a75342294a991e21157d7a1a61459cc176a2e966db7f40125b4af6deb7519da6`

```sql
SELECT id, n = ANY (SELECT n FROM bounds WHERE id IN (4,5) UNION ALL SELECT n FROM bounds WHERE id = 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n = ANY(SELECT n FROM bounds WHERE id IN (4, 5) UNION ALL SELECT n FROM bounds WHERE id = 5)
```

## 29. boundary quantified subqueries

Case ID: `22bff03203485c4ee6b37cf8307e703564e549b05a0679c851fe07d357194591`

```sql
SELECT id, n > ALL (SELECT n FROM bounds WHERE id IN (4,5) UNION ALL SELECT n FROM bounds WHERE id = 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n > ALL(SELECT n FROM bounds WHERE id IN (4, 5) UNION ALL SELECT n FROM bounds WHERE id = 5)
```

## 30. boundary quantified subqueries

Case ID: `e77177611920129e7fa679717598642caa275fb212bc7108319e9c2689ef30a6`

```sql
SELECT id, n <> ALL (SELECT n FROM bounds WHERE id IN (4,5) UNION ALL SELECT n FROM bounds WHERE id = 5) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: unsupported expression: n <> ALL(SELECT n FROM bounds WHERE id IN (4, 5) UNION ALL SELECT n FROM bounds WHERE id = 5)
```

## 31. boundary string storage

Case ID: `e91a77f56ddac4e75c57aa1dbf03608ec6b026e7c324a51d653d0ba32e266d66`

```sql
SELECT id, SUBSTRING(txt, -99) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 1):

```text
MySQL:  [{"Exact": "2"}, {"Exact": ""}]
Pintail: [{"Exact": "2"}, {"Exact": "NULL"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 32. boundary string storage

Case ID: `0c6bbb32dd60fce50442b2ef87407f90cb689d67a24c06941996767befecd91d`

```sql
SELECT id, LPAD(txt, 12, '') FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": ""}]
Pintail: [{"Exact": "1"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 33. boundary string grouping

Case ID: `3ae625104b0a0fd48d2b59ae2407f9969db8cffb95f6ebea09a1b9d7f2ce45e0`

```sql
SELECT txt, COUNT(*) FROM bounds GROUP BY txt;
```

MySQL returned 7 rows; Pintail returned 6 rows.

First differing row in the recorded output (zero-based index 2):

```text
MySQL:  [{"Exact": "a "}, {"Exact": "1"}]
Pintail: ["Null", {"Exact": "1"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 34. boundary string storage

Case ID: `57e26bb0b3bd30a41f6b4a7c89625c90e41b46c70a319f18815d3563d3f1287d`

```sql
SELECT id, SUBSTRING(fixed, -99) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 1):

```text
MySQL:  [{"Exact": "2"}, {"Exact": ""}]
Pintail: [{"Exact": "2"}, {"Exact": "NULL"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 35. boundary string storage

Case ID: `a1b875a2b94c8e0e90b8ccca1c459581ac32e22bad0cde45c62a447a729ab838`

```sql
SELECT id, LPAD(fixed, 12, '') FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": ""}]
Pintail: [{"Exact": "1"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 36. boundary string storage

Case ID: `68d871b6c8d5013aa7cb3fa9db6456bf4a1159fb8f415991895d3250c1c10a30`

```sql
SELECT id, raw LIKE 'a' FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 37. boundary string storage

Case ID: `d31257ea0be87b2c7734613de063bf8277585d960c635f871f438f8de32265b8`

```sql
SELECT id, SUBSTRING(raw, -99) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 38. boundary string storage

Case ID: `1f691f7dca47e91704fb9cb027378edc95aec15edc45b128843cfb0e00bdae98`

```sql
SELECT id, SUBSTRING(raw, 0) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 39. boundary string storage

Case ID: `154e7f204abff2f80ae960d56714a1224cfc139c775139b1f03427471465f595`

```sql
SELECT id, SUBSTRING(raw, 1, 0) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 40. boundary string storage

Case ID: `64ab413b812574c58ca3f89fbe80ffcb377285db32670c1040f86e2c0589060c`

```sql
SELECT id, LPAD(raw, 12, '') FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 41. boundary string storage

Case ID: `a70b5290a0e02dec299e9da7d4944f7cff83263acda75befc49f7e841fa5669a`

```sql
SELECT id, RPAD(raw, 0, 'x') FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 42. boundary string storage

Case ID: `c0c01c958fbd7f078b4195be555e97ee5964b9be2386e201a9476da661739a82`

```sql
SELECT id, CONCAT_WS(':', raw, 'end') FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 43. boundary string storage

Case ID: `d1f4f36eb1961015fbbc4a0cb0efdded7ed031023e7d40b794277650eea34ffe`

```sql
SELECT id, TRIM(raw) FROM bounds ORDER BY id;
```

Pintail execution error:

```text
execute: binary value is not valid UTF-8 for numeric coercion
```

## 44. boundary temporal precision

Case ID: `378152604de95393abf280f526f76840103ffb7915c210f057c2d15f9c3f5185`

```sql
SELECT id, dt0 = '2024-02-29' FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 1):

```text
MySQL:  [{"Exact": "2"}, {"Exact": "1"}]
Pintail: [{"Exact": "2"}, {"Exact": "0"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 45. boundary temporal precision

Case ID: `b085a923acbe0c2eec013ddf5c9a23d32148ed35374725dbeef4ca395bef52b1`

```sql
SELECT id, dt0 IN ('2024-02-29', NULL) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 1):

```text
MySQL:  [{"Exact": "2"}, {"Exact": "1"}]
Pintail: [{"Exact": "2"}, "Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 46. boundary temporal precision

Case ID: `e4d6b7473367612ca67ba01c0072f10ac3ee51fb514c97376dbd9c243ff0949d`

```sql
SELECT id, DATE_ADD(dt3, INTERVAL 1 MONTH) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "2000-03-29 00:00:00.125"}]
Pintail: [{"Exact": "1"}, {"Exact": "2000-03-29 00:00:00"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 47. boundary temporal precision

Case ID: `055e030e9fc6ce98665fb0514e88b06c4df2ad0539473723208bc64d50a75ef4`

```sql
SELECT id, DATE_SUB(dt3, INTERVAL 1 YEAR) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "1999-02-28 00:00:00.125"}]
Pintail: [{"Exact": "1"}, {"Exact": "1999-02-28 00:00:00"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 48. boundary temporal precision

Case ID: `55de3f4ff03d54f1a506eb61701369a0f140f5457f1a8f2e4516c737380bd5ee`

```sql
SELECT id, DATE_ADD(dt6, INTERVAL 1 MONTH) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "2000-03-29 00:00:00.125001"}]
Pintail: [{"Exact": "1"}, {"Exact": "2000-03-29 00:00:00"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 49. boundary temporal precision

Case ID: `b03bafd9d5ae917af70dc3d7fcd317d43b0d2d2be85437729eef6613388d1eef`

```sql
SELECT id, DATE_SUB(dt6, INTERVAL 1 YEAR) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "1999-02-28 00:00:00.125001"}]
Pintail: [{"Exact": "1"}, {"Exact": "1999-02-28 00:00:00"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 50. boundary temporal precision

Case ID: `ce3cdcac11f642df0dce0dd4136464c870b6beb2c5dd2274f70b566a6abef2e7`

```sql
SELECT id, DATE_ADD(stamp, INTERVAL 1 MONTH) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "2000-03-29 00:00:00.125001"}]
Pintail: [{"Exact": "1"}, {"Exact": "2000-03-29 00:00:00"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 51. boundary temporal precision

Case ID: `903ecdb69f60d64dff4d99391e332effc8c27e69748aa182b5087bbd86be14a0`

```sql
SELECT id, DATE_SUB(stamp, INTERVAL 1 YEAR) FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "1999-02-28 00:00:00.125001"}]
Pintail: [{"Exact": "1"}, {"Exact": "1999-02-28 00:00:00"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 52. boundary time durations

Case ID: `61d746ea6ebaf40bad2bf95cc09cf793f71ecc065f75ffc63b604001a6c6d7b2`

```sql
SELECT id, clock BETWEEN '-100:00:00' AND '100:00:00' FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "0"}]
Pintail: [{"Exact": "1"}, {"Exact": "1"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 53. boundary time durations

Case ID: `41c523c8cb1fb146e7f4af0d360cab1f60ed5c06882ffe3d0c60c2f4cdddbcf3`

```sql
SELECT id, clock + 0 FROM bounds ORDER BY id;
```

Pintail execution error:

```text
bind: operator + does not accept Some(Time64 { fsp: 6 }) and Some(Int64)
```

## 54. boundary time durations

Case ID: `66b31f917fb205903625bd56bdd2bc392d2225754bce64ad7730391358267d58`

```sql
SELECT id, ADDTIME(clock, '00:00:00.000001') FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 6):

```text
MySQL:  [{"Exact": "7"}, {"Exact": "838:59:59.000000"}]
Pintail: [{"Exact": "7"}, {"Exact": "838:59:59.000001"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 55. boundary time durations

Case ID: `7da955a3837cf3ddf07714797bd35ed0a5954424c0a6b2f28f5b94674fcd52b5`

```sql
SELECT id, SUBTIME(clock, '00:00:00.000001') FROM bounds ORDER BY id;
```

MySQL returned 8 rows; Pintail returned 8 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "1"}, {"Exact": "-838:59:59.000000"}]
Pintail: [{"Exact": "1"}, {"Exact": "-838:59:59.000001"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 56. boundary json identity

Case ID: `8278e0cdee5d685d40ccc4915c691476a3d5fe9dcb9c900d1ecd4b30e8aca686`

```sql
SELECT JSON_TYPE('9007199254740993');
```

MySQL returned 1 rows; Pintail returned 1 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": "UNSIGNED INTEGER"}]
Pintail: [{"Exact": "INTEGER"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 57. boundary result transport

Case ID: `b2a19d48ed7823abf94063525d7721c52c6e5a2f8654d64256bdb63472f925ba`

```sql
SELECT SUBSTRING('abc', -9);
```

MySQL returned 1 rows; Pintail returned 1 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": ""}]
Pintail: [{"Exact": "abc"}]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.

## 58. boundary result transport

Case ID: `fae988f04319f3a089bb36e5671728038337bf122eddc23f3700851699b31915`

```sql
SELECT LPAD('abc', 5, '');
```

MySQL returned 1 rows; Pintail returned 1 rows.

First differing row in the recorded output (zero-based index 0):

```text
MySQL:  [{"Exact": ""}]
Pintail: ["Null"]
```

For unordered queries the gate compares multisets; the row shown above is
diagnostic output, not an additional ordering requirement.
