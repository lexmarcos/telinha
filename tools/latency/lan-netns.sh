#!/bin/bash
# Runs a command inside an emulated home network, without root, to measure what a
# loopback run cannot: a real (non-loopback) path, a NAT router and mDNS.
#
#   tools/latency/lan-netns.sh <command...>
#
#   viewer  192.168.1.2   this namespace: the command, the harness, the viewer's Chrome,
#                         the signaling server (localhost:9000) and a STUN server
#   sender  192.168.1.3   second namespace: the native app or a second Chrome
#   router  192.168.1.1   bridges the two, masquerades toward the "internet" (no hairpin)
#   inet    198.51.100.10 a STUN server
#
# The command gets, as environment variables, wrappers that start a program inside the
# sender's namespace or as root without a sandbox:
#   LAN_NATIVE   $NATIVE_BIN started in the sender namespace (use as --nativo)
#   LAN_CHROME   Chrome for the viewer (set PUPPETEER_EXECUTABLE_PATH to it)
#   LAN_SENDER_CHROME   Chrome inside the sender namespace (--chrome-remetente)
# The sender reaches the viewer's localhost:9000 (signaling) and localhost:9001
# (static server: pass --porta 9001) through forwarders.
#
# Needs unshare, nsenter, ip, tc, iptables, socat and node. Environment:
#   NATIVE_BIN    the desktop app's binary (default: native/target/release/telinha)
#   CHROME_BIN    Chrome (default: puppeteer's)
#   DELAY_MS      one-way delay of each LAN link (default 0.5, so about 1 ms round trip)
#   WAN_DELAY_MS  one-way delay between router and internet (default 10)
#   MDNS_BLOCK=1  drop UDP 5353 on both hosts: browsers' ".local" names cannot resolve
#
# Example (native app as the streamer, viewer hiding its address like a real one):
#   cd tools/latency && ./lan-netns.sh bash -c 'PUPPETEER_EXECUTABLE_PATH=$LAN_CHROME \
#     node measure.mjs --app ../.. --server localhost:9000 --nativo $LAN_NATIVE --turbo --mdns'
# Run it from tools/latency. LD_LIBRARY_PATH for the native app is inherited.
if [ -z "$LAN_INNER" ]; then
  exec unshare -rn env LAN_INNER=1 "$0" "$@"
fi
here=$(cd "$(dirname "$0")" && pwd)
NATIVE_BIN=${NATIVE_BIN:-$here/../../native/target/release/telinha}
CHROME_BIN=${CHROME_BIN:-$(cd "$here" && node -e 'import("puppeteer").then(p=>console.log(p.default.executablePath()))')}
tmp=$(mktemp -d)
unshare -n sleep 100001 & SENDER=$!
unshare -n sleep 100002 & ROUTER=$!
unshare -n sleep 100003 & INET=$!
sleep 0.4
ns() { local n=$1; shift; nsenter -t "$n" -n "$@"; }
ip link set lo up
for n in $SENDER $ROUTER $INET; do ns $n ip link set lo up; done
ip link add a0 type veth peer name ra; ip link set ra netns $ROUTER
ip link add b0 type veth peer name rb; ip link set rb netns $ROUTER; ip link set b0 netns $SENDER
ip addr add 192.168.1.2/24 dev a0; ip link set a0 up; ip route add default via 192.168.1.1
ns $SENDER sh -c 'ip addr add 192.168.1.3/24 dev b0; ip link set b0 up; ip route add default via 192.168.1.1'
ns $ROUTER sh -c 'ip link add br0 type bridge; ip link set ra master br0; ip link set rb master br0; ip addr add 192.168.1.1/24 dev br0; ip link set br0 up; ip link set ra up; ip link set rb up; sysctl -qw net.ipv4.ip_forward=1'
ns $ROUTER ip link add wan0 type veth peer name in0; ns $ROUTER ip link set in0 netns $INET
ns $ROUTER sh -c 'ip addr add 198.51.100.1/24 dev wan0; ip link set wan0 up; ip route add default via 198.51.100.10'
ns $INET sh -c 'ip addr add 198.51.100.10/24 dev in0; ip link set in0 up'
ns $ROUTER iptables -t nat -A POSTROUTING -o wan0 -j MASQUERADE
ns $ROUTER tc qdisc add dev wan0 root netem delay ${WAN_DELAY_MS:-10}ms
ns $INET tc qdisc add dev in0 root netem delay ${WAN_DELAY_MS:-10}ms
tc qdisc add dev a0 root netem delay ${DELAY_MS:-0.5}ms
ns $SENDER tc qdisc add dev b0 root netem delay ${DELAY_MS:-0.5}ms
if [ "$MDNS_BLOCK" = 1 ]; then
  iptables -A INPUT -p udp --dport 5353 -j DROP
  ns $SENDER iptables -A INPUT -p udp --dport 5353 -j DROP
fi
printf '#!/bin/sh\nexec nsenter -t %s -n "%s" "$@"\n' $SENDER "$NATIVE_BIN" > $tmp/native
printf '#!/bin/sh\nexec "%s" --no-sandbox "$@"\n' "$CHROME_BIN" > $tmp/chrome
printf '#!/bin/sh\nexec nsenter -t %s -n "%s" --no-sandbox "$@"\n' $SENDER "$CHROME_BIN" > $tmp/chrome-sender
chmod +x $tmp/native $tmp/chrome $tmp/chrome-sender
# STUN on the internet side; the signaling server hands it out in /api/ice
nsenter -t $INET -n node "$here/stun.mjs" 0.0.0.0 3478 2>/dev/null & PIDS="$!"
export ICE_SERVERS='[{"urls":"stun:198.51.100.10:3478"}]'
node "$here/sinal.mjs" 9000 >/dev/null 2>&1 & PIDS="$PIDS $!"
# the sender's localhost:9000 and :9001 are the viewer's
for port in 9000 9001; do
  nsenter -t $SENDER -n socat TCP-LISTEN:$port,bind=127.0.0.1,fork,reuseaddr EXEC:"nsenter -t $$ -n socat - TCP\:127.0.0.1\:$port" & PIDS="$PIDS $!"
done
sleep 0.5
LAN_NATIVE=$tmp/native LAN_CHROME=$tmp/chrome LAN_SENDER_CHROME=$tmp/chrome-sender "$@"
rc=$?
kill $PIDS $SENDER $ROUTER $INET 2>/dev/null
pkill -P $$ 2>/dev/null
rm -rf "$tmp"
exit $rc
