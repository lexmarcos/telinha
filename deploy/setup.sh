#!/bin/sh
# Sets up and starts a Telinha server on a Linux VPS (Ubuntu or Debian).
# Run it again any time to update after a `git pull`.
#
#   sudo sh setup.sh
#
# Answers are saved in deploy/.env (never committed). Delete that file to start over.
set -eu
cd "$(dirname "$0")"
[ "$(id -u)" = 0 ] || { echo "Please run: sudo sh setup.sh"; exit 1; }

ask() { printf '%s ' "$1" >&2; read -r answer; echo "$answer"; }

if ! command -v docker >/dev/null 2>&1; then
  echo "Installing Docker..."
  curl -fsSL https://get.docker.com | sh
fi

if [ -f .env ]; then
  echo "Using the answers saved in deploy/.env"
else
  DOMAIN=$(ask "Your domain (example: telinha.example.com):")
  [ -n "$DOMAIN" ] || { echo "A domain is required."; exit 1; }
  PUBLIC=$(curl -4 -fsS https://api.ipify.org || hostname -I | awk '{print $1}')
  {
    echo "TURN_HOST=$DOMAIN"
    echo "PUBLIC_IP=$PUBLIC"
    echo "TURN_SECRET=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
    echo "TURN_TLS=off"
    echo "COMPOSE_FILE=docker-compose.simple.yml"
  } > .env
  case "$(ask "Set up the Discord bot now? (y/N):")" in
    y|Y|yes|s|S|sim)
      {
        echo "COMPOSE_PROFILES=discord"
        echo "BOT_URL=http://bot:8790"
        echo "DISCORD_CLIENT_ID=$(ask "Application ID:")"
        echo "DISCORD_PUBLIC_KEY=$(ask "Public Key:")"
        echo "DISCORD_TOKEN=$(ask "Bot Token:")"
        echo "DISCORD_CLIENT_SECRET=$(ask "Client Secret:")"
      } >> .env
      ;;
  esac
  chmod 600 .env
fi
. ./.env

# The domain must point to this server, or HTTPS cannot be set up.
SEEN=$(getent ahostsv4 "$TURN_HOST" | awk 'NR==1 {print $1}')
if [ "$SEEN" != "$PUBLIC_IP" ]; then
  echo "Warning: $TURN_HOST points to '${SEEN:-nothing}', but this server is $PUBLIC_IP."
  echo "Create an A record for $TURN_HOST with the value $PUBLIC_IP at your domain provider."
  ask "Press Enter to continue anyway, or Ctrl+C to stop." >/dev/null
fi

# The site
mkdir -p public
cp ../index.html ../style.css ../app.js ../stats.js ../stats.css ../turbo.js ../som-linux.conf public/

# Relay settings. Cloud servers often sit behind NAT: coturn then needs both addresses.
LOCAL=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}')
EXTERNAL=$PUBLIC_IP
[ -n "$LOCAL" ] && [ "$LOCAL" != "$PUBLIC_IP" ] && EXTERNAL="$PUBLIC_IP/$LOCAL"
sed -e "s|__TURN_SECRET__|$TURN_SECRET|" -e "s|__TURN_HOST__|$TURN_HOST|" -e "s|^external-ip=.*|external-ip=$EXTERNAL|" \
  -e '/^listening-ip=/d' -e '/^tls-listening-port=/d' -e '/^cert=/d' -e '/^pkey=/d' -e 's/^syslog$/log-file=stdout/' -e '/^no-cli$/d' \
  turnserver.conf > turnserver.generated.conf
printf 'no-tls\nno-dtls\n' >> turnserver.generated.conf

# Open the ports if the server's own firewall (ufw) is on.
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
  ufw allow 80,443/tcp >/dev/null
  ufw allow 443/udp >/dev/null
  ufw allow 3478 >/dev/null
  ufw allow 49160:49999/udp >/dev/null
fi

docker compose up -d --build --remove-orphans

echo
echo "Done! Open https://$TURN_HOST"
if [ "${COMPOSE_PROFILES:-}" = discord ]; then
  echo
  echo "Discord bot: in the Developer Portal, paste these values."
  echo "  General Information > Interactions Endpoint URL:  https://$TURN_HOST/discord/interacoes"
  echo "  OAuth2 > Redirects:                              https://$TURN_HOST/discord/login/volta"
  echo "Then add the bot to your Discord server with this link:"
  echo "  https://discord.com/oauth2/authorize?client_id=$DISCORD_CLIENT_ID&scope=bot+applications.commands&permissions=3072"
fi
