# Development

## Running the dashboard against a live server

In production `pintail` serves the built dashboard itself, so the app calls the
API with relative paths — `/api/...` and `/status`. Under `nuxt dev` the app has
its own origin on port 3000, and those paths would hit Nuxt rather than the
engine. `nuxt.config.ts` therefore proxies them in development to whatever
`PINTAIL_API_URL` names, defaulting to `http://127.0.0.1:8080`.

The proxy carries `/api/events` too, which is a server-sent event stream: the
dashboard's live updates depend on it not being buffered.

### Against a server on this machine

```sh
cargo run -p pintail --bin pintail -- --data-dir .devdata --http-bind 127.0.0.1:8080
cd packages/dashboard && bun run dev
```

Nothing else to configure — the default target matches.

### Against a server in Docker

A container publishes its port on the *Docker host*. If the daemon is local,
publish 8080 and the default target already reaches it:

```sh
docker run -d --name pintail-dev -p 8080:8080 <image> \
  --data-dir /var/lib/pintail --http-bind 0.0.0.0:8080
cd packages/dashboard && bun run dev
```

If the daemon is remote — a `docker context` over SSH — then `localhost:8080`
on your machine is not the container's port. Forward it first, then point the
proxy at the local end of the tunnel:

```sh
ssh -N -L 8080:127.0.0.1:8080 <your-docker-host> &
PINTAIL_API_URL=http://127.0.0.1:8080 bun run dev
```

Any reachable address works the same way:

```sh
PINTAIL_API_URL=http://10.0.0.5:8080 bun run dev
```

### Checking the proxy rather than guessing

With both running, these should answer from the engine, not from Nuxt:

```sh
curl -s localhost:3000/status
# {"status":"ready","version":"0.1.0",...}

curl -s localhost:3000/api/databases
# {"error":"Bearer authentication is required"}
```

That 401 is the point: it is pintail's own response. HTML back from either
means the proxy is not reaching the server — check `PINTAIL_API_URL`, and check
the tunnel is still up if you are using one.

Sign in through the dashboard as usual; the first-boot admin credentials are
printed by the server on its first start.

### Do not point this at a shared deployment

The dashboard is a control plane: it can start snapshots, change replication
modes, rotate keys and delete databases. Run it against a server you started
for yourself. In particular, never point it at the deployed compose stack —
`docs/limitations.md` describes the dashboard as a local control plane and not
a multi-tenant security boundary, and that assumption is what makes it safe.
