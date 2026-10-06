#!/bin/sh
# Copia o certificado que o Caddy emitiu para TURN_HOST para o coturn (porta
# 5349, TURN sobre TLS) e reinicia o coturn quando ele muda. Roda pelo cron.
set -e
. /opt/telinha/.env # TURN_HOST e CADDY_CERTS (pasta certificates do volume de dados do Caddy)
CRT=$(find "${CADDY_CERTS:?}" -path "*/$TURN_HOST/$TURN_HOST.crt" 2>/dev/null | head -1)
[ -n "$CRT" ] || exit 0
KEY="${CRT%.crt}.key"
DST=/etc/coturn/certs
if ! cmp -s "$CRT" "$DST/fullchain.pem"; then
  install -o root -g turnserver -m 644 "$CRT" "$DST/fullchain.pem"
  install -o root -g turnserver -m 640 "$KEY" "$DST/privkey.pem"
  systemctl restart coturn
  echo "certificado do coturn atualizado"
fi
