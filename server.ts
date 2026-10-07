// Static server for local testing: bun server.ts
const port = Number(process.env.PORT ?? 5180);
const root = import.meta.dir;

Bun.serve({
  port,
  async fetch(req) {
    const path = new URL(req.url).pathname;
    const file = Bun.file(root + (path === "/" ? "/index.html" : path));
    if (path.includes("..") || !(await file.exists())) return new Response("Não encontrado", { status: 404 });
    return new Response(file);
  },
});
console.log(`Telinha at http://localhost:${port}`);
