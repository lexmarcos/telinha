// node summarize.mjs results.jsonl [more.jsonl...]
// Per-run table plus mean / stdev / range of p50 and p95 across runs, grouped by relay flag.
import fs from "node:fs";

const rows = process.argv.slice(2).flatMap((f) => fs.readFileSync(f, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l)));
const f = (x) => (x == null ? "-" : String(x));
console.log(["label", "frames", "fail", "p50", "p95", "p99", "mean", "fps", "jbMs", "res", "enc", "qlr", "after15 p50", "pair"].join("\t"));
for (const r of rows) {
  console.log([r.label, r.frames, r.decodeFailures, r.p50, r.p95, r.p99, r.mean, r.fps, r.jitterBufferMsPerFrame, r.extra?.recvResolution,
    r.extra?.encodeMsPerFrame, r.qualityLimitationReason, f(r.extra?.after15s?.p50), (r.extra?.candidatePairs || [])[0]].map(f).join("\t"));
}
const stat = (xs) => {
  const m = xs.reduce((a, b) => a + b, 0) / xs.length;
  const sd = Math.sqrt(xs.reduce((a, b) => a + (b - m) ** 2, 0) / Math.max(1, xs.length - 1));
  return `mean ${m.toFixed(1)}  sd ${sd.toFixed(1)}  range ${Math.min(...xs)}..${Math.max(...xs)}`;
};
for (const relay of [false, true]) {
  const g = rows.filter((r) => r.relay === relay && r.p50 != null);
  if (g.length < 2) continue;
  console.log(`\n${relay ? "relay" : "direct"} (${g.length} runs)`);
  console.log(`  p50: ${stat(g.map((r) => r.p50))}`);
  console.log(`  p95: ${stat(g.map((r) => r.p95))}`);
}
