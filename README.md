# Telinha

Share your screen with friends. Open the site, get a 4 digit channel, send the link. No accounts.

## Host your own

You need a server (VPS) with Ubuntu or Debian and a domain name.

1. At your domain provider, create an **A record** that points your domain to the server's IP.
2. Connect to the server and run:
   ```sh
   git clone https://github.com/lexmarcos/telinha
   sudo sh telinha/deploy/setup.sh
   ```
3. Type your domain when asked. When it says **Done**, open `https://your.domain`.

If your server provider has a firewall page, open these ports: **80**, **443**, **3478** (TCP and UDP) and **49160 to 49999** (UDP).

To update: `cd telinha && git pull && sudo sh deploy/setup.sh`

## Discord bot (optional)

Only people in your Discord voice call can watch, and a **Watch** button shows up in the call chat.

1. Open the [Discord Developer Portal](https://discord.com/developers/applications) and click **New Application**.
2. Write down 4 values:
   * **Application ID** and **Public Key** in General Information
   * **Token** in Bot (click Reset Token)
   * **Client Secret** in OAuth2 (click Reset Secret)
3. On the server, run `sudo rm telinha/deploy/.env` and then `sudo sh telinha/deploy/setup.sh`. Answer **y** to the Discord question and paste the 4 values.
4. At the end it shows 2 addresses and 1 link. Paste each address where it says in the Developer Portal, then open the link to add the bot to your Discord server.

## Desktop app (optional)

Streams with less delay using your graphics card (Linux and Windows). Get it on the **Releases** page. On first run, paste an invite link from your site.

To build an app that already knows your server, on Linux with Rust and Docker installed:

```sh
cd telinha/native
sh scripts/fetch-deps.sh
echo "TELINHA_SERVER=your.domain" > .env.build
sh scripts/package.sh tudo
```

The files show up in `native/dist`.

## Already have a reverse proxy?

Use `deploy/docker-compose.yml` instead of the setup script:

1. Copy `deploy/.env.example` to `deploy/.env` and fill it in.
2. Add `deploy/Caddyfile.snippet` to your Caddy.
3. Install coturn (`apt install coturn`) and make `/etc/turnserver.conf` from `deploy/turnserver.conf`, using your `.env` values for the `__FIELDS__`.
4. Run `deploy/turn-certs.sh` daily (cron) so coturn gets Caddy's certificate.
5. Copy the site files (`*.html`, `*.css`, `*.js`, `som-linux.conf`) to `deploy/public` and run `docker compose up -d` in `deploy`.

## Develop

Run `bun server.ts` and open http://localhost:5180. Add `?servidor=your.domain` to the address to use your server.

How it works, delay and audio: [docs/how-it-works.md](docs/how-it-works.md).
