# Vaxelium: server

The chat server. It holds channels, messages, roles, pins, presence and direct messages in
SQLite, and checks every login with the auth service.

It also serves the web client on its own port, so people can use Vaxelium in a browser. The
web client is the desktop client compiled to WebAssembly. Build it with the main repository's
`scripts/build-web.sh`.

## Running

```
server [ADDR] [DB_PATH] [AUTH_ADDR] [COUNT]
```

- `ADDR` defaults to `127.0.0.1:7878`.
- `DB_PATH` defaults to `vaxelium.db`.
- `AUTH_ADDR` defaults to `127.0.0.1:7900`, the auth service.
- `COUNT` starts a group of servers on consecutive ports.

The web client is served on `VAXELIUM_WEB_ADDR`, which defaults to the first port after the
group (7880 for one server on 7878). Set it to `off` to turn the web client off. `VAXELIUM_WEB_DIR` says
where the built files are.

Settings are read from a `.env` file in the folder it starts in. See the main repository's
`.env.example`.

## Part of Vaxelium

This repository is one part of the Vaxelium project. The rest are linked from the main
repository:

- [Vaxelium](https://github.com/Vaxelium/Vaxelium): the workspace, build scripts and docs
- [common](https://github.com/Vaxelium/common): the wire protocol shared by every part
- [auth](https://github.com/Vaxelium/auth): the auth service that owns every account
- [client](https://github.com/Vaxelium/client): the desktop and web client

## Building

This crate builds on its own. Clone it and run:

```
cargo test
```

It fetches `common` from [its GitHub repository](https://github.com/Vaxelium/common).
Inside the main repository, the workspace uses the local `common` folder instead, so
changes to `common` are tested with the crates that use it.


## Licence

GNU GPL v3. See [LICENSE](LICENSE).
