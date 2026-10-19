// Browser-only mock backend (used when not running inside Tauri), with
// realistic Turkish fixtures so every UI state can be rendered and reviewed.

import { asciiFold, matchRanges, queryTerms, turkishLower } from "../lib/turkish";
import type { Backend } from "./client";
import { FILE_KINDS } from "./types";
import type {
  AppInfo,
  DocumentView,
  FailureInfo,
  FileKind,
  FolderInfo,
  IndexProgress,
  OcrLanguageInfo,
  PageImage,
  PageView,
  UpdateInfo,
  SearchHit,
  SearchResponse,
  TextSource,
} from "./types";

interface MockPage {
  text: string;
  source: TextSource;
  confidence: number | null;
  error?: string;
}
interface MockDoc {
  path: string;
  folderId: number;
  kind: FileKind;
  size: number;
  modified: number;
  pages: MockPage[];
}

const DAY = 86_400;
const NOW = Math.floor(Date.now() / 1000);
const FLOOR = 0.6;

const page = (text: string, source: TextSource = "text_layer", confidence: number | null = null): MockPage => ({
  text,
  source,
  confidence,
});
const failedPage = (error: string): MockPage => ({ text: "", source: "ocr", confidence: null, error });
const isFailed = (p: MockPage) => p.error !== undefined;

const DOCS_TR: MockDoc[] = [
  {
    path: "C:\\Arşiv\\Sözleşmeler\\2026\\Kira Sözleşmesi - İnci Pusulası Tasarım Ltd. Şti. (Ankara Çankaya Ofis).pdf",
    folderId: 1, kind: "pdf", size: 1_842_331, modified: NOW - 3 * DAY,
    pages: [
      page("KİRA SÖZLEŞMESİ\nMadde 1. Taraflar\nİşbu kira sözleşmesi, Ankara'da mukim İnci Pusulası Tasarım Ltd. Şti. ile kiracı Gökçe Yıldırım arasında 12.10.2026 tarihinde imzalanmıştır.", "ocr", 0.91),
      page("Madde 2. Kira bedeli\nAylık kira bedeli 18.500,00 TL olup her ayın 5'ine kadar ödenir. Sözleşmenin süresi bir yıldır; taraflar sözleşmeyi yenileme hakkına sahiptir.", "ocr", 0.88),
    ],
  },
  {
    path: "C:\\Arşiv\\Sözleşmeler\\2026\\Hizmet Sözleşmesi - Çağlayan Lojistik A.Ş.docx",
    folderId: 1, kind: "word", size: 48_210, modified: NOW - 12 * DAY,
    pages: [page("HİZMET SÖZLEŞMESİ\nÇağlayan Lojistik A.Ş. ile Işık Bilişim arasında depolama ve dağıtım hizmetlerine ilişkin sözleşmedir. Hizmet bedeli %18 KDV hariçtir.", "native")],
  },
  {
    path: "C:\\Arşiv\\Sözleşmeler\\Eski\\Teslim Tutanağı - Isparta Şubesi.pdf",
    folderId: 1, kind: "pdf", size: 912_004, modified: NOW - 220 * DAY,
    pages: [
      page("TESLİM TUTANAĞI\nISPARTA ŞUBESİ\nAşağıda dökümü yapılan demirbaşlar eksiksiz teslim alınmıştır.", "ocr", 0.42),
      page("İmza ve kaşe alanı. El yazısı notlar okunamadı.", "ocr", 0.31),
      failedPage("Tesseract timed out after 120 s"),
    ],
  },
  {
    path: "C:\\Arşiv\\Sözleşmeler\\2026\\Taranmış Dilekçe - Öğrenci İşleri.jpg",
    folderId: 1, kind: "image", size: 2_302_118, modified: NOW - 1 * DAY,
    pages: [page("DİLEKÇE\nÖğrenci İşleri Daire Başkanlığına\nSözleşmeli personel olarak görev yapmaktayım; izin talebimin değerlendirilmesini arz ederim.", "ocr", 0.79)],
  },
  {
    path: "D:\\Muhasebe\\Faturalar\\2026-10\\Fatura FTR-2026-004821 Şimşek Gıda.pdf",
    folderId: 2, kind: "pdf", size: 204_551, modified: NOW - 5 * DAY,
    pages: [page("FATURA\nFatura No: FTR-2026-004821\nŞimşek Gıda San. ve Tic. Ltd. Şti.\nToplam: 1.234,56 TL (KDV %18 dahil)\nPlaka: 34 ABC 123")],
  },
  {
    path: "D:\\Muhasebe\\Faturalar\\2026-10\\Bütçe Tablosu 2026 Q3.xlsx",
    folderId: 2, kind: "excel", size: 88_120, modified: NOW - 20 * DAY,
    pages: [
      page("Özet\nKalem\tTutar\nKira giderleri\t222.000,00\nPersonel\t1.480.000,00", "native"),
      page("Detay\nİstanbul ofisi kira sözleşmesi yenileme farkı\t36.000,00", "native"),
    ],
  },
  {
    path: "C:\\Arşiv\\Sözleşmeler\\Notlar\\Kira artışı notları.txt",
    folderId: 1, kind: "text", size: 1_204, modified: NOW - 2 * DAY,
    pages: [page("Kira artışı TÜFE oranında yapılacak. Sözleşme yenileme tarihi: 12.10.2026.", "native")],
  },
  {
    path: "D:\\Muhasebe\\Faturalar\\Makbuzlar\\Makbuz - Ödeme Tahsilat 0012.png",
    folderId: 2, kind: "image", size: 640_000, modified: NOW - 45 * DAY,
    pages: [page("TAHSİLAT MAKBUZU\nÜç bin iki yüz Türk lirası nakden tahsil edilmiştir.", "ocr", 0.83)],
  },
  {
    path: "D:\\Muhasebe\\Faturalar\\2025\\Faaliyet Raporu 2025 - Yönetim Kurulu.pdf",
    folderId: 2, kind: "pdf", size: 5_110_993, modified: NOW - 400 * DAY,
    pages: [
      page("FAALİYET RAPORU 2025\nYönetim kurulu, şirketin yıllık faaliyetlerini ve sözleşmelerimizin durumunu değerlendirmiştir."),
      page("Bölüm 3. Finansal Tablolar\nNet satışlar bir önceki yıla göre %12 artmıştır."),
      page("Bölüm 4. Riskler\nKur riski ve tedarik zinciri gecikmeleri izlenmektedir."),
    ],
  },
];

// English fixtures for `?demo=en` (README screenshots).
const DOCS_EN: MockDoc[] = [
  {
    path: "C:\\Archive\\Contracts\\2026\\Office Lease Agreement - Northwind Studio (Seattle).pdf",
    folderId: 1, kind: "pdf", size: 1_842_331, modified: NOW - 3 * DAY,
    pages: [
      page("OFFICE LEASE AGREEMENT\nSection 1. Parties\nThis lease agreement is made on October 12, 2026 between Harbor Point Properties LLC (landlord) and Northwind Studio Inc. (tenant) for Suite 410, 1200 Pine Street, Seattle.", "ocr", 0.93),
      page("Section 2. Rent\nMonthly rent is $6,850.00, due on the 5th day of each month. The term is twelve months; the tenant may renew the lease for another year with 60 days' written notice.", "ocr", 0.9),
      page("Section 3. Security deposit\nA security deposit of $13,700.00 is held by the landlord and returned within 30 days after the lease ends.", "ocr", 0.89),
    ],
  },
  {
    path: "C:\\Archive\\Contracts\\2026\\Service Agreement - Bluewater Logistics.docx",
    folderId: 1, kind: "word", size: 48_210, modified: NOW - 12 * DAY,
    pages: [page("SERVICE AGREEMENT\nBluewater Logistics and Northwind Studio agree on warehousing and delivery services. Fees exclude sales tax. Either party may terminate with 30 days' notice.", "native")],
  },
  {
    path: "C:\\Archive\\Contracts\\Notes\\Lease renewal notes.txt",
    folderId: 1, kind: "text", size: 1_204, modified: NOW - 2 * DAY,
    pages: [page("Lease renewal: ask Harbor Point for a 3% cap on the rent increase. Renewal notice due by August 13, 2027.", "native")],
  },
  {
    path: "C:\\Archive\\Contracts\\2026\\Signed letter - Lease extension request.jpg",
    folderId: 1, kind: "image", size: 2_302_118, modified: NOW - 1 * DAY,
    pages: [page("Dear Ms. Carter,\nWe would like to extend our lease for Suite 410 by twelve months under the same terms. Please confirm the renewal at your earliest convenience.\nSincerely, Daniel Brooks", "ocr", 0.86)],
  },
  {
    path: "D:\\Accounting\\Invoices\\2026-10\\Invoice INV-2026-004821 Cedar Foods.pdf",
    folderId: 2, kind: "pdf", size: 204_551, modified: NOW - 5 * DAY,
    pages: [page("INVOICE\nInvoice No: INV-2026-004821\nCedar Foods Co.\nTotal: $1,234.56 (tax included)\nPayment terms: net 30")],
  },
  {
    path: "D:\\Accounting\\Invoices\\2026-10\\Budget 2026 Q3.xlsx",
    folderId: 2, kind: "excel", size: 88_120, modified: NOW - 20 * DAY,
    pages: [
      page("Summary\nItem\tAmount\nRent\t82,200.00\nPayroll\t480,000.00", "native"),
      page("Detail\nSeattle office lease renewal difference\t3,600.00", "native"),
    ],
  },
  {
    path: "D:\\Accounting\\Invoices\\Receipts\\Receipt - Rent payment October.png",
    folderId: 2, kind: "image", size: 640_000, modified: NOW - 25 * DAY,
    pages: [page("RECEIPT\nReceived from Northwind Studio Inc. the sum of $6,850.00 for October rent, Suite 410.", "ocr", 0.84)],
  },
  {
    path: "D:\\Accounting\\Reports\\Annual Report 2025 - Board of Directors.pdf",
    folderId: 2, kind: "pdf", size: 5_110_993, modified: NOW - 400 * DAY,
    pages: [
      page("ANNUAL REPORT 2025\nThe board reviewed the company's yearly activity and the status of our contracts, including the office lease."),
      page("Part 3. Financial statements\nNet sales grew 12% over the previous year."),
    ],
  },
];

const DEMO = new URLSearchParams(location.search).get("demo");
const DOCS: MockDoc[] = DEMO === "en" ? DOCS_EN : DOCS_TR;

const kindOf = (path: string): FileKind => {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  if (ext === "pdf") return "pdf";
  if (ext === "docx") return "word";
  if (ext === "xlsx" || ext === "xlsm") return "excel";
  if (ext === "txt") return "text";
  return "image";
};

const FAILURES: FailureInfo[] = [
  {
    path: "C:\\Arşiv\\Sözleşmeler\\Eski\\şifreli-teklif.pdf",
    error: "Cannot open PDF: password required",
    reason: "encrypted",
    at: NOW - 3600,
    failedPages: null,
    pageCount: null,
  },
  {
    path: "D:\\Muhasebe\\Faturalar\\bozuk-tablo.xlsx",
    error: "Cannot open workbook: invalid zip header",
    reason: "damaged",
    at: NOW - 7200,
    failedPages: null,
    pageCount: null,
  },
];

const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

const xml = (text: string) => text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

/** A fake scanned page (SVG) carrying the page text, for the "Sayfa" tab. */
function mockPageImage(text: string, page: number, width: number): PageImage {
  const height = Math.round(width * 1.414);
  const lines = text.split("\n").flatMap((line) => line.match(/.{1,64}(\s|$)/g) ?? [line]);
  const body = lines
    .map((line, i) => `<text x="56" y="${96 + i * 22}" font-family="Georgia, serif" font-size="15" fill="#222">${xml(line.trim())}</text>`)
    .join("");
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 595 842"><rect width="595" height="842" fill="#fbfaf7"/>${body}</svg>`;
  return { page, width, height, dataUrl: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}` };
}

export function createMockBackend(): Backend {
  let ocr: OcrLanguageInfo = {
    language: new URLSearchParams(location.search).get("ocr") ?? "tur",
    available: ["tur", "eng", "tur+eng"],
    pendingRereads: 0,
  };
  let folders: FolderInfo[] = [];
  let nextId = 1;
  // `?demo` starts with both folders already indexed (screenshots).
  if (DEMO !== null) {
    const roots = DEMO === "en" ? ["C:\\Archive\\Contracts", "D:\\Accounting"] : ["C:\\Arşiv\\Sözleşmeler", "D:\\Muhasebe\\Faturalar"];
    folders = roots.map((path) => ({
      id: nextId++, path, fileCount: 0, indexedCount: 0, failedCount: 0, lowConfidenceCount: 0, partialCount: 0,
      lastScan: null, missing: false, kinds: [...FILE_KINDS],
    }));
  }
  const progressHandlers = new Set<(p: IndexProgress) => void>();
  const changedHandlers = new Set<(n: number) => void>();
  let progress: IndexProgress = {
    phase: "idle", total: 0, done: 0, failed: 0, current: null, startedAt: null, finishedAt: null, watching: false,
    cancelling: false, cancelled: false, embedDone: 0, embedTotal: 0, unread: 0, pendingEmbed: 0,
  };
  // Read but not yet in the semantic index (a cancel stops the embed).
  const unembedded = new Set<string>();
  let cancelRequested = false;
  // Paths read so far; a cancelled run keeps them, a rescan only does the rest.
  const indexed = new Set<string>();
  // Fixture documents belong to folder 1 or 2 whatever path was picked;
  // fixture failures by path, as before.
  const folderOf = (path: string) => {
    const doc = DOCS.find((d) => d.path === path);
    return doc ? folders.find((f) => f.id === doc.folderId) : folders.find((f) => path.startsWith(f.path));
  };
  const selected = (path: string) => folderOf(path)?.kinds.includes(kindOf(path)) ?? false;
  const visible = () => DOCS.filter((d) => indexed.has(d.path) && selected(d.path));

  const emit = (update: Partial<IndexProgress>) => {
    progress = { ...progress, ...update };
    progressHandlers.forEach((h) => h(progress));
  };

  const refreshCounts = (folderId: number, lastScan: number | null) => {
    folders = folders.map((f) => {
      if (f.id !== folderId) return f;
      const docs = DOCS.filter((d) => d.folderId === folderId && selected(d.path));
      const done = docs.filter((d) => indexed.has(d.path));
      const failures = FAILURES.filter((x) => folderOf(x.path)?.id === folderId && selected(x.path) && indexed.has(x.path));
      return {
        ...f,
        fileCount: docs.length + FAILURES.filter((x) => folderOf(x.path)?.id === folderId && selected(x.path)).length,
        indexedCount: done.length,
        failedCount: failures.length,
        lowConfidenceCount: done.filter((d) => d.pages.some((p) => lowConfidence(p))).length,
        partialCount: done.filter((d) => d.pages.some(isFailed)).length,
        pendingEmbedCount: done.filter((d) => unembedded.has(d.path)).length,
        lastScan: lastScan ?? f.lastScan,
      };
    });
  };

  /** Returns false when the run was cancelled. */
  const runIndex = async (folderId: number): Promise<boolean> => {
    const root = folders.find((x) => x.id === folderId)?.path ?? "\0";
    const docs = DOCS.filter((d) => d.folderId === folderId && selected(d.path)).map((d) => d.path);
    const failures = FAILURES.filter((f) => f.path.startsWith(root) && selected(f.path)).map((f) => f.path);
    const todo = [...docs, ...failures].filter((path) => !indexed.has(path));
    emit({
      phase: "scanning", total: 0, done: 0, failed: 0, current: null, startedAt: Math.floor(Date.now() / 1000),
      finishedAt: null, cancelled: false, textDone: 0, embedDone: 0, embedTotal: 0, unread: 0, pendingEmbed: null,
    });
    await delay(400);
    emit({ phase: "indexing", total: todo.length });
    let failed = 0;
    let read = 0;
    let embedded = 0;
    for (const [i, path] of todo.entries()) {
      if (cancelRequested) break;
      await delay(new URLSearchParams(location.search).has("slow") ? 2500 : 450);
      if (cancelRequested) break; // the file being read is dropped, not half-written
      indexed.add(path);
      read = i + 1;
      if (failures.includes(path)) failed += 1;
      else unembedded.add(path);
      // The semantic index trails the text by one file; a cancel leaves it pending.
      const previous = todo[i - 1];
      if (previous && unembedded.delete(previous)) embedded += 1;
      emit({ done: read, textDone: read, failed, current: path, embedDone: embedded, embedTotal: read - failed });
    }
    if (!cancelRequested) for (const path of todo) if (unembedded.delete(path)) embedded += 1;
    emit({ embedDone: embedded, unread: cancelRequested ? todo.length - read : 0 });
    emit({ phase: "committing" });
    await delay(250);
    refreshCounts(folderId, Math.floor(Date.now() / 1000));
    const cancelled = cancelRequested;
    emit({ phase: "idle", current: null, finishedAt: Math.floor(Date.now() / 1000), cancelling: false, cancelled, pendingEmbed: unembedded.size });
    changedHandlers.forEach((h) => h(visible().length));
    return !cancelled;
  };

  const runAll = async (ids: number[]) => {
    cancelRequested = false;
    for (const id of ids) if (!(await runIndex(id))) break;
    cancelRequested = false;
  };

  const lowConfidence = (p: MockPage) => p.source === "ocr" && !isFailed(p) && (p.confidence ?? 0) < FLOOR;

  if (DEMO !== null) {
    for (const doc of DOCS) indexed.add(doc.path);
    for (const folder of folders) refreshCounts(folder.id, NOW - 600);
    progress = { ...progress, watching: true, finishedAt: NOW - 600 };
  }

  return {
    appInfo: async (): Promise<AppInfo> => ({
      version: "0.3.0",
      ocrReady: !new URLSearchParams(location.search).has("noocr"),
      ocrDetail: new URLSearchParams(location.search).has("noocr")
        ? "Tesseract model not found: C:\\Program Files\\Zoria Arşiv\\tessdata\\tur.traineddata"
        : "tesseract v5.5.3.20260724",
      pdfReady: true,
      pdfDetail: "pdfium chromium/8100",
      semanticReady: true,
      semanticDetail: "EmbeddingGemma 2 (q4, 768d) verified",
      indexDir: "C:\\Users\\kullanici\\AppData\\Local\\Zoria Arşiv\\index",
      documentCount: visible().length,
    }),
    // `?missing` marks the second folder as gone, to review that state.
    listFolders: async () =>
      folders.map((f) => ({ ...f, missing: new URLSearchParams(location.search).has("missing") && f.id === 2 })),
    addFolder: async (path, kinds) => {
      if (kinds.length === 0) throw { code: "kinds_empty", message: "Select at least one file kind" };
      const folder: FolderInfo = {
        id: nextId++, path, fileCount: 0, indexedCount: 0, failedCount: 0, lowConfidenceCount: 0, partialCount: 0,
        lastScan: null, missing: false, kinds: FILE_KINDS.filter((k) => kinds.includes(k)),
      };
      folders = [...folders, folder];
      emit({ watching: true });
      void runAll([folder.id]);
      return folder;
    },
    setFolderKinds: async (id, kinds) => {
      if (kinds.length === 0) throw { code: "kinds_empty", message: "Select at least one file kind" };
      const before = folders.find((f) => f.id === id);
      if (!before) throw { code: "folder_missing", message: String(id) };
      folders = folders.map((f) => (f.id === id ? { ...f, kinds: FILE_KINDS.filter((k) => kinds.includes(k)) } : f));
      // Unselected kinds leave the index at once; new kinds need a scan.
      for (const path of [...indexed]) if (folderOf(path)?.id === id && !selected(path)) indexed.delete(path);
      refreshCounts(id, null);
      changedHandlers.forEach((h) => h(visible().length));
      if (kinds.some((k) => !before.kinds.includes(k)) && progress.phase === "idle") void runAll([id]);
      return folders.find((f) => f.id === id) as FolderInfo;
    },
    cancelIndexing: async () => {
      if (progress.phase === "idle") return false;
      cancelRequested = true;
      emit({ cancelling: true });
      return true;
    },
    removeFolder: async (id) => {
      for (const path of [...indexed]) if (folderOf(path)?.id === id) indexed.delete(path);
      folders = folders.filter((f) => f.id !== id);
      emit({ watching: folders.length > 0 });
      changedHandlers.forEach((h) => h(visible().length));
    },
    reindex: async (folderId) => {
      void runAll(folders.filter((f) => folderId === null || f.id === folderId).map((f) => f.id));
    },
    completeEmbeddings: async () => {
      const todo = [...unembedded];
      if (todo.length === 0) return;
      cancelRequested = false;
      emit({ phase: "embedding", total: todo.length, done: 0, failed: 0, embedDone: 0, embedTotal: todo.length, unread: 0, cancelled: false, pendingEmbed: null });
      for (const [i, path] of todo.entries()) {
        if (cancelRequested) break;
        await delay(600);
        unembedded.delete(path);
        emit({ done: i + 1, embedDone: i + 1, current: path });
      }
      folders.forEach((f) => refreshCounts(f.id, null));
      const cancelled = cancelRequested;
      cancelRequested = false;
      emit({ phase: "idle", current: null, cancelling: false, cancelled, pendingEmbed: unembedded.size });
    },
    indexStatus: async () => progress,
    retryFailures: async (folderId) => {
      const count = FAILURES.filter((f) => indexed.has(f.path) && (folderId === null || folderOf(f.path)?.id === folderId)).length;
      for (const f of FAILURES) if (folderId === null || folderOf(f.path)?.id === folderId) indexed.delete(f.path);
      void runAll(folders.filter((f) => folderId === null || f.id === folderId).map((f) => f.id));
      return count;
    },
    listFailures: async (folderId) => {
      const inScope = (path: string) =>
        indexed.has(path) && selected(path) && (folderId === null || folderOf(path)?.id === folderId);
      const partial: FailureInfo[] = DOCS.filter((d) => inScope(d.path) && d.pages.some(isFailed)).map((d) => ({
        path: d.path,
        error: d.pages.find(isFailed)?.error ?? "",
        at: NOW - 600,
        failedPages: d.pages.filter(isFailed).length,
        pageCount: d.pages.length,
        reason: "ocr_failed" as const,
      }));
      return [...partial, ...FAILURES.filter((f) => inScope(f.path))];
    },
    search: async (request): Promise<SearchResponse> => {
      const started = performance.now();
      const terms = queryTerms(request.query);
      if (!terms.length) throw { code: "query_empty", message: "Query has no searchable words" };
      await delay(120);
      const scored = visible()
        .filter((d) => !request.kinds.length || request.kinds.includes(d.kind))
        .filter((d) => request.modifiedAfter === null || d.modified >= request.modifiedAfter)
        .filter((d) => request.modifiedBefore === null || d.modified <= request.modifiedBefore)
        .map((d) => {
          const perPage = d.pages.map((p) => matchRanges(p.text, terms).length);
          const best = perPage.indexOf(Math.max(...perPage));
          return { d, score: perPage.reduce((a, b) => a + b, 0), best };
        })
        .filter((x) => x.score > 0)
        .sort((a, b) => b.score - a.score);
      const hits: SearchHit[] = scored.slice(request.offset, request.offset + request.limit).map(({ d, score, best }) => {
        const text = (d.pages[best]?.text ?? "").replace(/\s+/g, " ");
        return {
          path: d.path,
          fileName: d.path.split("\\").pop() ?? d.path,
          folder: d.path.split("\\").slice(0, -1).join("\\"),
          kind: d.kind,
          size: d.size,
          modified: d.modified,
          pageCount: d.pages.length,
          lowConfidencePages: d.pages.filter(lowConfidence).length,
          ocrPages: d.pages.filter((p) => p.source === "ocr" && !isFailed(p)).length,
          failedPages: d.pages.filter(isFailed).length,
          semanticOnly: false,
          score,
          snippets: [{ page: best, text, highlights: matchRanges(text, terms) }],
        };
      });
      // File-name matches the content did not find come first, like the engine.
      const fold = (t: string) => asciiFold(turkishLower(t)).replace(/['’]/g, "");
      const named = visible()
        .filter((d) => !scored.some((x) => x.d.path === d.path))
        .filter((d) => !request.kinds.length || request.kinds.includes(d.kind))
        .filter((d) => terms.every((t) => fold(d.path.split("\\").pop() ?? "").includes(fold(t))));
      const all = [...named.map((d) => ({ d, score: 0, best: 0, nameOnly: true })), ...scored.map((x) => ({ ...x, nameOnly: false }))];
      const page = all.slice(request.offset, request.offset + request.limit).map(({ d, score, best, nameOnly }) => {
        const text = nameOnly ? "" : (d.pages[best]?.text ?? "").replace(/\s+/g, " ");
        return {
          path: d.path,
          fileName: d.path.split("\\").pop() ?? d.path,
          folder: d.path.split("\\").slice(0, -1).join("\\"),
          kind: d.kind,
          size: d.size,
          modified: d.modified,
          pageCount: d.pages.length,
          lowConfidencePages: d.pages.filter(lowConfidence).length,
          ocrPages: d.pages.filter((p) => p.source === "ocr" && !isFailed(p)).length,
          failedPages: d.pages.filter(isFailed).length,
          semanticOnly: false,
          nameOnly,
          score,
          snippets: text ? [{ page: best, text, highlights: matchRanges(text, terms) }] : [],
        } satisfies SearchHit;
      });
      void hits;
      return { total: all.length, hits: page, tookMs: Math.round(performance.now() - started) };
    },
    getDocument: async (path, query): Promise<DocumentView> => {
      const d = DOCS.find((x) => x.path === path);
      if (!d) throw { code: "not_indexed", message: path };
      await delay(80);
      const terms = queryTerms(query);
      const pages: PageView[] = d.pages.map((p, index) => ({
        index, text: p.text, source: p.source, confidence: p.confidence,
        lowConfidence: lowConfidence(p), highlights: matchRanges(p.text, terms), error: p.error ?? null,
      }));
      return { path: d.path, fileName: d.path.split("\\").pop() ?? d.path, kind: d.kind, size: d.size, modified: d.modified, pages };
    },
    getPageImage: async (path, page, width) => {
      const d = DOCS.find((x) => x.path === path);
      if (!d) throw { code: "not_indexed", message: path };
      const p = d.pages[page];
      if ((d.kind !== "pdf" && d.kind !== "image") || !p) throw { code: "preview_unavailable", message: path };
      await delay(150);
      return mockPageImage(p.error ? "" : p.text, page, width);
    },
    // `?update` simulates a published release.
    checkUpdate: async (): Promise<UpdateInfo> => {
      const available = new URLSearchParams(location.search).has("update");
      return { enabled: true, available, version: available ? "0.2.0" : null, notes: available ? "Hata düzeltmeleri" : null };
    },
    installUpdate: async () => {
      await delay(800);
    },
    openPath: async () => {},
    revealPath: async () => {},
    openUrl: async (url) => {
      window.open(url, "_blank", "noopener");
    },
    // Dev server serves the generated files straight from the repo (never bundled).
    legalTexts: async () => {
      const text = (path: string) => fetch(path).then((r) => r.text()).then((t) => t.replace(/^\uFEFF/, ""));
      const [notices, terms, termsEn, license] = await Promise.all([
        fetch("/resources/licenses/third-party.json").then((r) => r.json()),
        text("/resources/licenses/KULLANIM_KOSULLARI.txt"),
        text("/resources/licenses/TERMS.txt"),
        text("/LICENSE"),
      ]);
      return {
        notices,
        noticesFile: "C:\\Users\\Kullanıcı\\AppData\\Local\\Zoria Arşiv\\licenses\\THIRD_PARTY_NOTICES.txt",
        terms,
        termsEn,
        license,
      };
    },
    // `?ocr=eng` starts with another document language.
    ocrLanguage: async () => ({ ...ocr }),
    setOcrLanguage: async (language) => {
      await delay(120);
      if (!ocr.available.includes(language)) throw { code: "ocr_language_unavailable", message: language };
      const scans = DOCS.filter((d) => d.pages.some((p) => p.source === "ocr")).length;
      ocr = { ...ocr, language, pendingRereads: language === ocr.language ? ocr.pendingRereads : scans };
      return { ...ocr };
    },
    pickFolders: async () =>
      (window.prompt("Klasör yolları (; ile ayırın)", "C:\\Arşiv\\Sözleşmeler") ?? "")
        .split(";")
        .map((p) => p.trim())
        .filter(Boolean),
    onProgress: async (handler) => {
      progressHandlers.add(handler);
      return () => progressHandlers.delete(handler);
    },
    onChanged: async (handler) => {
      changedHandlers.add(handler);
      return () => changedHandlers.delete(handler);
    },
  };
}
