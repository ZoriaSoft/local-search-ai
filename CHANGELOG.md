# Changelog

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
