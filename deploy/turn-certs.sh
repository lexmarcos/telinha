#!/bin/sh
# Copies the certificate Caddy issued for TURN_HOST to coturn (port
# 5349, TURN over TLS) and restarts coturn when it changes. Runs from cron.
set -e
. /opt/telinha/.env # TURN_HOST and CADDY_CERTS (certificates folder of the Caddy data volume)
CRT=$(find "${CADDY_CERTS:?}" -path "*/$TURN_HOST/$TURN_HOST.crt" 2>/dev/null | head -1)
[ -n "$CRT" ] || exit 0
KEY="${CRT%.crt}.key"
DST=/etc/coturn/certs
if ! cmp -s "$CRT" "$DST/fullchain.pem"; then
  install -o root -g turnserver -m 644 "$CRT" "$DST/fullchain.pem"
  install -o root -g turnserver -m 640 "$KEY" "$DST/privkey.pem"
  systemctl restart coturn
  echo "coturn certificate updated"
fi
