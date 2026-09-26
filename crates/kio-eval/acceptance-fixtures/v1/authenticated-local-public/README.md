# A08 authenticated local public inputs

`ocr.pdf` is the repository-authored one-page provider OCR fixture. `image.png`
is page 1 rasterized by Poppler at 100 DPI; it carries the same public marker so
both PDF and standalone-image OCR must produce real text. Neither file contains
private user content. The standalone image must also be embedded and opened
byte-for-byte through the release CLI.

The ordered bundle digest hashes `filename + NUL + bytes + NUL` in this order:
`ocr.pdf`, `image.png`. SHA-256: `8a61250a8a21ac49d3edaa03b55e5a9e0b45e7bda92f678c53811ffc080e2cc3`.

This fixture supplies inputs, not successful execution evidence. The phased
executor writes a checkpoint after OCR and a final receipt only after real
local embedding, image opening, and empty-ledger assertions complete.
