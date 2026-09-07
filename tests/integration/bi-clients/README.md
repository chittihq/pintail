# BI client gate

Run `bun install --frozen-lockfile && bun run gate` on the Linux build host
with the repository's Docker connection configured. The gate builds the
recovery-profile binary there and packages it into a temporary runtime image.
It creates an isolated network and three containers, then removes those
containers, their volumes, the network and the temporary image on exit.

Metabase v0.63.16 connects through its MySQL driver to a synthetic replica,
synchronizes its schema, and executes two saved questions: monthly counts
and a numeric filter. Each answer is checked against fixed fixture values.
The receipt is `results.json`; full logs are retained by the `bi-clients`
stage in `validate-out/runs/<run>/`.

The RC and stable profiles also run the wire-client matrix before this smoke.
That matrix requires Java 21+, Go, Bun, uv and the MySQL CLI. The JDBC client
pins Connector/J 9.6.0 by SHA-256, uses server-side prepared statements and
INFORMATION_SCHEMA discovery, and checks tables, columns, primary keys,
decimal scale, datetime precision and aggregate nullability. `PINTAIL_JAVA`
and `PINTAIL_MYSQL_CLI` may select installed executable paths.

This is an application and driver smoke, not an automated desktop UI test.
