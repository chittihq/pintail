# Real-data differential corpora

Query files for the public MySQL sample datasets, one query per line,
runnable against a live pair with `bun run scripts/qcheck.ts --file <file>`
after loading the matching dataset (sakila / world / employees aka
datacharmer test_db) into the qcheck MySQL container via `--seed` or by
loading the upstream dumps manually.

These are the batches that found #261 (SET bitmask ordering) and #262
(inner-join aggregate zero-count groups) on their first run, and that
verified the fixes byte-exact: sakila 12/12, employees 8/8 at 2.8M rows,
world 4/4. MySQL is the oracle; a diff is a Pintail bug until MySQL is
proven wrong.

## sakila-db.sql.gz

The full sakila schema + data dump (schema and data files concatenated),
vendored from https://downloads.mysql.com/docs/sakila-db.tar.gz so the
browser soak suite can load a real dataset without a network fetch at gate
time. Sakila is published by Oracle under the New BSD license.
