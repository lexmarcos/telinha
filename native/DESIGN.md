# Design system do Telinha nativo

O app nativo é a mesma marca do site num formato menor: uma **telinha de bolso**.
Uma bolha redonda fica flutuando sobre o que a pessoa está fazendo (um jogo, uma
planilha), mostra o canal e quem está assistindo, e some na bandeja quando não é
necessária. Tudo que não for a bolha só aparece quando alguém pede.

Este documento é a fonte das decisões. A interface é feita em Slint e espelha
cada item em `ui/tokens.slint` (valores) e nos componentes de `ui/` (peças); as
molas, que o app anima quadro a quadro, ficam em `src/ui/motion.rs`. Nenhuma
tela define cor, tamanho, raio ou tempo de animação próprios: se faltar um
valor, ele entra aqui primeiro.

## Princípios

1. **A bolha é o produto.** É a única coisa sempre visível, então é onde está a
   personalidade: um tubo de TV redondo com o número do canal em matriz de pontos
   e a luz vermelha de "no ar". O resto (menu, painel) é quieto e funcional.
2. **Não atrapalhar o que está embaixo.** A janela tem só o tamanho do conteúdo,
   sem bordas e sem fundo. Nada pisca nem se mexe sozinho, exceto a luz de "no ar"
   quando a transmissão começa (um único momento).
3. **Tudo nasce da bolha e volta para ela.** Menu e painel de qualidade abrem
   ancorados na bolha, crescendo a partir dela, e fecham pelo mesmo caminho.
4. **Resposta no aperto.** Botões e a bolha reagem no clique, não no soltar.
   Animações usam molas interrompíveis: um clique no meio de uma animação começa
   do ponto onde a coisa está, nunca pula.
5. **As palavras dizem o que acontece.** "Transmitir a tela", "Parar de
   transmitir", "Copiar convite". Frase curta, verbo no começo, sem caixa alta.

## Cores

| Token | Valor | Uso |
|---|---|---|
| `room` | `#0D1430` | Fundo dos painéis (a sala escura do site) |
| `room_raised` | `#16204A` | Item em foco/hover, trilho do controle segmentado |
| `screen` | `#070B1D` | Vidro do tubo da bolha |
| `phosphor` | `#CFE0FF` | Números do canal, marcador selecionado, foco |
| `text` | `#EDF0FF` | Texto principal |
| `muted` | `#8C95BD` | Texto de apoio, legendas |
| `line` | `#C8D4FF` a 14% | Contornos finos |
| `tally` | `#FF4438` | Só "no ar": anel da bolha, parar transmissão |
| `warn` | `#FFC56B` | Aviso (conexão ruim, codificador por software) |

Avatares de quem assiste usam a mesma paleta do site, escolhida pelo nome:
`#CFE0FF #FFD9A8 #BFF0D4 #F5C6FF #FFE48A #A8ECFF #FFC2C2`.

O vermelho é reservado: aparece apenas quando a tela está sendo transmitida
(anel, ponto de gravação, botão de parar). Se tudo está vermelho, nada está.

## Tipografia

- **Interface:** fonte do sistema (sem fonte embutida), para ficar em casa no
  Windows e no Linux.
- **Números do canal:** **Doto** (matriz de pontos, a mesma do site), embutida.
  Só para dígitos do canal; nunca para texto.

| Token | Tamanho | Peso | Uso |
|---|---|---|---|
| `title` | 15 | semibold | Título do painel |
| `body` | 14 | regular / semibold em itens | Itens de menu, botões |
| `label` | 12 | semibold | Nome de grupo ("Resolução") |
| `note` | 12 | regular | Explicações curtas, em `muted` |
| `digits` | 17 na bolha, 30 no campo do canal | Doto black | Número do canal |

Rótulos em frase normal, nunca em caixa alta.

## Espaço, tamanho e forma

- Grade de 4: `xs 4`, `s 8`, `m 12`, `l 16`, `xl 24`.
- **Bolha:** 64 de diâmetro; tubo interno 52; anel de "no ar" de 3.
- **Pontinhos de quem assiste:** 18 de diâmetro, sobrepostos em 5, no máximo 5 e
  depois `+n`. Ficam numa fileira centralizada embaixo da bolha.
- **Painel:** largura 264, raio 20, respiro interno 14. Sai da bolha com 10 de
  distância, alinhado ao topo dela.
- **Item de menu:** altura 36, raio 10, ícone de 16 à esquerda.
- **Controle segmentado:** altura 32, raio 11 no trilho e 9 no marcador.
- **Botão:** altura 36, formato de pílula.

Raios seguem a hierarquia: superfície grande (painel 20) > controle (11) >
marcador (9). Nunca um raio único para tudo.

## Material e profundidade

A janela não desfoca o que está atrás dela, então o "vidro" do site vira um
material sólido escuro com três camadas:

1. Fundo `room` a 96% de opacidade, contorno `line` de 1.
2. Luz batendo na borda de cima: uma linha de 1 que acende no meio e some nas
   pontas (branco até 16%).
3. Sombra larga para fora, feita de sete anéis cada vez maiores e mais fracos
   (`ui/shadow.slint`), porque o desenho por software não tem `drop-shadow`.
   Painel: 16 de alcance, 8 para baixo, 60% somados. Bolha: 10, 4 e 50%.

Superfícies maiores têm sombra maior (painel > menu > bolha). Nunca empilhar
material translúcido sobre material translúcido.

## Movimento

Molas, não durações. Parâmetros no estilo Apple (amortecimento, resposta):

| Token | Amortecimento | Resposta | Uso |
|---|---|---|---|
| `spring_ui` | 1.0 | 0.32 s | Painel abrindo e fechando, marcador do segmentado |
| `spring_press` | 1.0 | 0.12 s | Encolher no aperto (0,94) e voltar |
| `spring_tally` | 0.7 | 0.45 s | O anel vermelho chegando quando a transmissão começa |
| `spring_fade` | 1.0 | 0.22 s | Conteúdo novo aparecendo quando o painel troca |

As molas rodam no app (`src/ui/spring.rs`), não nas curvas do Slint: uma mola
que muda de alvo no meio do caminho continua da posição **e da velocidade**
atuais, então abrir e fechar rápido nunca pula nem "bate na parede". As curvas
do Slint ficam só para micro-respostas (`press` 90 ms, `hover` 120 ms).

- O painel **nasce da bolha**: escala de 0,92 a 1 com origem no lado da bolha,
  e opacidade de 0 a 1. Fecha pelo mesmo caminho.
- Toda animação parte do valor atual: abrir e fechar rápido não pula.
- **Troca de painel** (menu → qualidade): a altura da superfície vai com
  `spring_ui` até a do painel novo, e o conteúdo novo aparece com `spring_fade`.
  A janela reserva a maior das duas alturas durante a troca e só encolhe no
  fim, para mudar de tamanho duas vezes em vez de a cada quadro.
- O painel mede a si mesmo (o layout do Slint dá a altura natural); nada é
  medido "no olho" nem guardado em cache.
- `Esc` fecha o painel.
- **Movimento reduzido** (configuração do sistema): sem escala nem quique, só
  troca de opacidade curta.

## Componentes

Cada componente vive em `ui/*.slint` e só lê valores de `ui/tokens.slint`. O
estado vem do app por `AppState` (`ui/state.slint`).

### Bolha (`bubble`)
Tubo redondo com o número do canal em Doto. Estados:
- **Sem canal:** um "+" discreto no vidro, convidando a clicar.
- **Conectando:** arco de `phosphor` girando devagar em volta (o único giro do app).
- **No canal:** número do canal aceso em `phosphor`.
- **No ar:** anel `tally` de 3 + ponto de gravação no alto à direita.
- **Hover:** contorno mais claro. **Aperto:** encolhe para 0,94.
Clique abre ou fecha o menu. Arrastar move a janela pelo gerenciador de janelas
(acompanha o mouse 1:1); o arraste só começa depois de 4 de movimento, para não
confundir com clique.

### Pontinhos de quem assiste (`dots`)
Círculos de 18 com as iniciais, cor do avatar pelo nome. Quem está num navegador
que não recebe o modo nativo aparece apagado (40%). Os nomes aparecem na linha
de apoio do menu quando são até três ("Canal 4821. Ana e Bia assistindo"); com
mais gente, a quantidade.

### Painel (`panel`)
Container com o material. Cabeçalho opcional com título e, em subpainéis, a
seta de voltar à esquerda (mesmo lugar sempre).

### Item de menu (`menu_item`)
Ícone + texto, ocupa a largura. Variantes: `normal`, `primary` (texto em
`phosphor`) e `danger` (texto em `tally`, só para parar transmissão). Hover pinta
`room_raised`.

### Controle segmentado (`segmented`)
Trilho `room_raised` com marcador que desliza com `spring_ui` até a opção
escolhida. O marcador tem a mesma luz de topo do material, em pequeno.

### Botões (`button`)
- `primary`: fundo `text`, texto `room`. Uma ação principal por painel.
- `quiet`: fundo `line`, texto `text`.
- `danger`: fundo `tally`, texto branco.
No aperto encolhe para 0,97 na hora; no hover clareia um pouco.

### Campo do canal (`channel_field`)
Os quatro dígitos em Doto 30 sobre `screen`, como a telinha da página inicial do
site. Aceita também um link de convite colado.

### Nota (`note`)
Texto `note` em `muted`; em aviso, ícone e texto em `warn`.

## Texto

- Verbos no começo e a mesma palavra do botão até a confirmação: "Transmitir a
  tela" → "Transmitindo"; "Copiar convite" → "Convite copiado".
- Erros dizem o que houve e o que fazer: "O canal 4821 não está no ar. Confira o
  número com quem te convidou."
- Sem reticências decorativas, sem pontos de exclamação, sem emoji.
