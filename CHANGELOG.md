# Changelog

## 0.3.0 — 2026-10-19

First public release.

- English interface next to Turkish; the app follows the Windows language and
  can be switched in Settings.
- Document language for scanned pages: Turkish, English or both. Changing it
  reads earlier scans again in the background.
- English plural and verb forms match their base form in search.
- About page with the Apache-2.0 license, terms of use and every bundled
  open-source component with its license text.

Privacy rebuild:
- Windows release builds remap local user-profile and source paths and check
  the resulting executable before distribution.
- Unicode-folder and plate-style search fixtures use neutral synthetic data.
- The initial public history and release tag are replaced with a sanitized
  snapshot. Existing clones should be recloned.
