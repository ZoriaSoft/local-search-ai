import type { IndexProgress } from "../api/types";

// The progress bar's fill (0..1) for one progress snapshot.
//
// - Semantic index rebuild only ("Şimdi tamamla" / backfill, phase
//   "embedding"): files embedded / files to embed, plus the running file's
//   chunk share so a slow first file does not sit at 0.
// - Scan with a model: the mean of the two stages, text read
//   (textDone / total) and semantic index (embedDone / embedTotal, plus the
//   chunk share of the file being embedded). While files are
//   still unread, each unread file is counted as one that will need an
//   embed, so the second stage's share does not drop as reading adds files
//   to it.
// - Scan without a model: text read only.
// - Committing: full.
export function progressRatio(p: IndexProgress, semantic: boolean): number {
  if (p.phase === "idle" || p.phase === "scanning") return 0;
  if (p.phase === "committing") return 1;
  const embedTotal = p.embedTotal ?? 0;
  const embedDone = p.embedDone ?? 0;
  if (p.phase === "embedding") {
    if (embedTotal <= 0) return 0;
    // A file whose embed failed still counts as processed (`done`).
    const files = Math.max(embedDone, p.done);
    return clamp((files + chunkShare(p, files, embedTotal)) / embedTotal);
  }
  if (p.total <= 0) return 0;
  const textDone = Math.min(Math.max(p.textDone ?? 0, p.done), p.total);
  const text = textDone / p.total;
  if (!semantic && embedTotal === 0) return clamp(text);
  const expected = embedTotal + (p.total - textDone);
  const embed = expected > 0 ? (embedDone + chunkShare(p, embedDone, embedTotal)) / expected : 1;
  return clamp((text + embed) / 2);
}

// The running file's finished chunks as a share of one file; only while
// files are left (that file is not counted in `files` yet).
function chunkShare(p: IndexProgress, files: number, embedTotal: number): number {
  return files < embedTotal && p.chunksTotal ? clamp((p.chunksDone ?? 0) / p.chunksTotal) : 0;
}

// What the bar shows: never moves back within one run (`run` = the run's
// start stamp); a new run starts from its own value.
export interface ShownRatio {
  run: number | null;
  ratio: number;
}

export function holdRatio(prev: ShownRatio | null, run: number | null, ratio: number): ShownRatio {
  if (!prev || prev.run !== run) return { run, ratio: clamp(ratio) };
  return { run, ratio: Math.max(prev.ratio, clamp(ratio)) };
}

function clamp(x: number): number {
  return Number.isFinite(x) ? Math.min(1, Math.max(0, x)) : 0;
}
