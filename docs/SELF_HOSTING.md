# Self-hosting with Docker

This runs MochiPrep as a container instead of the desktop app. The data is kept in a Docker volume.

> **Status:** the Dockerfile and `compose.yaml` have not yet been built or run on a Docker host. Build the image once and check it before relying on it.

## Run on this machine

You need Docker Engine 28.0.0 or newer with Compose v2, and git. Older Docker Engine versions let other machines on the same local network reach ports published to `127.0.0.1`, so the default setup is not host-only there. Check with `docker version`.

```bash
git clone https://github.com/rinnyanneko/MochiPrep.git
cd MochiPrep
docker compose up -d
```

Open <http://localhost:8787>. The port is published on `127.0.0.1` only, so this is the only machine that can reach it.

Common commands:

```bash
docker compose logs -f      # follow the log
docker compose down         # stop; the data volume is kept
docker compose up -d --build   # rebuild after git pull
```

The data lives in the `mochiprep-data` volume. Deleting it (`docker compose down -v`) deletes your accounts and history.

Docker runs MochiPrep in a container. The desktop app ([Install](../README.md#install)) is the simpler option on Windows and macOS.

## Access from another device (unsafe)

> **Read this before you do it.** MochiPrep's only protection for the API is a token. The page gets that token from `/api/session`, and that endpoint does not require an Origin header, only an allowed `Host` header. A client on your network can set that header to anything MochiPrep accepts, read the token, and then call the whole API. That includes placing orders on an armed account, changing agents and providers, and changing settings. Keys stored in the database are still not shown in API responses.
>
> Only do this on a network you fully trust, such as a home LAN behind your router, and never on a shared or public network. Do not forward the port on your router or expose it to the internet. If you are not sure, keep the default.

To allow another device, you need to do two things:

1. Tell MochiPrep to listen beyond the loopback address with `MOCHIPREP_HOST=0.0.0.0`. The image already sets this.
2. Add the address that device will use to `MOCHIPREP_ALLOWED_ORIGINS`. Browsers and the Host check both use this list.

In `compose.yaml`, change the port line and add the origin:

```yaml
    ports:
      - "8787:8787"            # every interface, instead of 127.0.0.1:8787
    environment:
      MOCHIPREP_ALLOWED_ORIGINS: "http://192.168.1.20:8787"   # your machine's LAN address
```

Then open `http://192.168.1.20:8787` on the other device. If your address changes, update the origin.

The desktop app never does this. It removes `MOCHIPREP_HOST` from the backend's environment, so the desktop backend always listens on loopback.

For a source build, set the same variables in your shell, for example:

```bash
MOCHIPREP_HOST=0.0.0.0 MOCHIPREP_ALLOWED_ORIGINS=http://192.168.1.20:8787 pnpm start
```

The server prints a warning at startup when it listens beyond the loopback address.
