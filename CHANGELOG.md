# Changelog

## 0.4.0 — 2026-11-25

- More file types: PowerPoint and OpenDocument presentations, OpenDocument
  text and spreadsheets, RTF, legacy Excel (.xls), HTML, e-mail (.eml) with
  its attachments, and Turkish official documents (UYAP .udf, e-Yazışma
  .eyp; for an encrypted .eyp only its metadata is searchable).
- Timed folder updates: a folder can be indexed only on chosen days and
  hours. Work stops when the window ends and continues in the next one.
- Search operators: required (+, AND/VE), excluded (-, NOT/DEĞİL), OR/VEYA,
  nearby words (NEAR/5, YAKIN/5), wildcards (* and ?), similar spelling
  (word~, word~2) and full-weight synonyms (word&).
- Size filter next to the date filter.
- A "How to search" guide with examples that run on a click.

## 0.3.1 — 2026-11-25

- The installer now works without an internet connection: the Microsoft
  Edge WebView2 Runtime is included and installed only when Windows does not
  already have it.
- The Visual C++ runtime that semantic search needs is shipped next to the
  app, so a clean Windows install no longer misses it.
- Indexing keeps its progress across restarts, and document search returns
  consistent results.

## 0.3.0 — 2026-10-19

First public release.

- English interface next to Turkish; the app follows the Windows language and
  can be switched in Settings.
- Document language for scanned pages: Turkish, English or both. Changing it
  reads earlier scans again in the background.
- English plural and verb forms match their base form in search.
- About page with the Apache-2.0 license, terms of use and every bundled
  open-source component with its license text.
