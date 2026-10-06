# Telinha

Compartilhamento de tela entre amigos pelo navegador, usando PeerJS (WebRTC).

Uma pessoa abre um canal e recebe um número de 4 dígitos. Os outros digitam o número ou abrem o link de convite. Qualquer pessoa no canal pode transmitir, e várias pessoas podem transmitir ao mesmo tempo.

## Rodar localmente

```sh
bun server.ts        # http://localhost:5180
```

## Publicar

O app é estático, mas a sinalização e o repasse (TURN) rodam num servidor seu: o app usa o próprio endereço de onde foi aberto. Rodando em `localhost`, ele usa o PeerJS público (só STUN); para testar localmente contra o seu servidor, abra com `?servidor=seu.dominio`.

A pasta `deploy/` tem tudo para uma VPS com Docker e um proxy reverso (Caddy) já rodando:

- `server.js` + `Dockerfile` + `docker-compose.yml`: container `telinha` em `/opt/telinha`, com o site (`/opt/telinha/public`), a sinalização do PeerJS em `/peer` e credenciais temporárias do TURN em `/api/ice`. Ele entra na rede Docker do proxy (`PROXY_NETWORK`).
- `.env.example`: copie para `/opt/telinha/.env` e preencha (segredo do TURN, domínio, IP público, rede do proxy, pasta de certificados do Caddy). O `.env` nunca vai para o repositório.
- `Caddyfile.snippet`: bloco para o Caddyfile do proxy.
- `turnserver.conf`: modelo do coturn (instalado via apt). Gere o `/etc/turnserver.conf` trocando os `__CAMPOS__` pelos valores do `.env`:

  ```sh
  . /opt/telinha/.env
  sed -e "s/__TURN_SECRET__/$TURN_SECRET/" -e "s/__PUBLIC_IP__/$PUBLIC_IP/" -e "s/__TURN_HOST__/$TURN_HOST/" \
    /opt/telinha/turnserver.conf > /etc/turnserver.conf
  ```
- `turn-certs.sh`: copia o certificado do Caddy para o coturn (TURN sobre TLS na porta 5349). Instale em `/usr/local/bin/telinha-turn-certs` e rode pelo cron todo dia.

Atualizar só o site:

```sh
scp index.html style.css app.js stats.js stats.css turbo.js som-linux.conf root@SUA_VPS:/opt/telinha/public/
```

Portas abertas no firewall: 3478 udp/tcp, 5349 tcp e 49160–49999 udp.

## Como funciona

- Quem abre o canal registra o id `telinha-canal-v1-NNNN` no servidor de sinalização e mantém a lista de quem está na sala.
- Os outros se conectam a essa pessoa por um canal de dados e recebem a lista.
- Quem transmite liga direto para cada pessoa da sala. O vídeo vai de ponta a ponta, sem passar por quem abriu o canal. Quando a conexão direta falha (CGNAT, por exemplo), o coturn da VPS repassa os dados, que continuam criptografados.
- Se a VPS não responder, o app usa o servidor público do PeerJS, só com STUN. Nesse modo não há repasse: os servidores TURN do PeerJS não existem mais.
- Se quem abriu o canal sair, o canal sai do ar.

## Latência

Ideias tiradas do Sunshine/Moonlight:

- Quem transmite descobre se a placa de vídeo codifica H.264, VP9 ou AV1 (`mediaCapabilities`) e avisa quem assiste, que põe esse codec na frente da resposta.
- O teto de bitrate acompanha a resolução e os quadros por segundo escolhidos (cerca de 0,08 bit por pixel), em vez de um valor fixo. No repasse isso reduziu o atraso mediano de 300 para cerca de 216 ms no teste.
- Quem transmite escolhe resolução, quadros por segundo e prioridade (fluidez ou nitidez) no botão de qualidade.
- O painel de conexão (tecla I) mostra atraso estimado, codec, buffer, decodificação e se o caminho é direto ou por repasse.

### Atraso mínimo (WebCodecs)

Opção "Atraso: Mínimo" no painel de qualidade (`turbo.js`). É o caminho do Sunshine no navegador:

- Quem transmite lê os quadros crus da captura (`MediaStreamTrackProcessor`), codifica uma vez só com `VideoEncoder` (modo `realtime`, taxa constante, H.264 na placa de vídeo quando houver, keyframe só quando alguém pede) e manda os mesmos pacotes para todo mundo.
- O vídeo vai por um canal de dados sem ordem e com reenvio de no máximo 150 ms, aberto sobre a conexão do PeerJS. Pedidos de keyframe e relatórios vão pelo canal confiável.
- Quem assiste remonta, decodifica com `optimizeForLatency` e entrega o quadro na hora para uma trilha comum (`MediaStreamTrackGenerator`), sem buffer de espera. O áudio continua pelo WebRTC.
- Quem assiste manda um relatório por segundo. Se a pessoa recebe bem menos do que foi enviado três vezes seguidas, ela volta para o WebRTC, que se adapta à banda. Quem usa navegador sem as APIs (fora Chrome e Edge) recebe pelo WebRTC desde o começo.

Medido em conexão direta, mesma qualidade (720p60): mediana de 45 a 48 ms no modo mínimo contra 66 a 80 ms no normal, e p95 de 74 a 78 ms contra 83 a 97 ms. Pelo repasse com os dois lados na mesma internet de casa, a banda não aguentou a taxa fixa e a pessoa voltou para o WebRTC em uns 3 segundos, como esperado.

`tools/latency` mede o atraso de ponta a ponta com dois Chromes automatizados: a tela falsa desenha o horário em blocos e quem assiste lê esses blocos do vídeo.

```sh
cd tools/latency && npm install
EXTRA="--warmup 15 --server SEU_DOMINIO" ./run-suite.sh ../.. novo.jsonl novo 3 1 20
EXTRA="--warmup 15 --server SEU_DOMINIO --turbo" ./run-suite.sh ../.. turbo.jsonl turbo 3 0 20   # modo atraso mínimo
node summarize.mjs novo.jsonl
```

Os números do Chrome sem interface usam codec por software e os dois lados na mesma máquina, então servem para comparar versões, não como valor absoluto.

## Som

- **Windows:** ao escolher "Tela inteira", marque "Compartilhar áudio do sistema" no seletor do Chrome.
- **Aba do navegador:** o som da aba vai junto em qualquer sistema.
- **Linux:** o Chrome só manda som de abas. A saída de som vira uma entrada virtual "Som do computador" (`som-linux.conf`, um loopback do PipeWire que acompanha a saída padrão), e o Telinha pega essa entrada como microfone. Instalar uma vez:

  ```sh
  mkdir -p ~/.config/pipewire/pipewire.conf.d && curl -fsSL https://SEU_DOMINIO/som-linux.conf -o ~/.config/pipewire/pipewire.conf.d/telinha-som.conf && systemctl --user restart pipewire
  ```

  O painel de qualidade mostra esse comando já com o endereço certo. Depois da primeira vez (que pede permissão de microfone), o som entra sozinho ao compartilhar. Dá para desligar no painel de qualidade. Vai tudo que sai no fone, inclusive vozes de uma chamada no Discord.
- **Mac:** precisa de um dispositivo virtual como o BlackHole.
- Quem assiste recebe Opus em estéreo a até 192 kb/s (o padrão do navegador é voz mono).

## Limites

- Cada pessoa que assiste recebe uma cópia do vídeo de quem transmite, então o upload de quem transmite limita o tamanho do grupo. Para 4 ou 5 pessoas funciona bem.
- Celulares conseguem assistir, mas a maioria não consegue transmitir.
