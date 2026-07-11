# Feature: <nama fitur>

<!--
Konvensi feature-flow (lihat feature-flow.md di repo propagator):
  1. Copy file ini ke  ~/work/features/<slug>/prd.md  dan isi.
  2. Jalankan  /feature-impact <slug>   → hasil: features/<slug>/impact.md (REVIEW dulu).
  3. Jalankan  /feature-tests <slug>    → draft PBT + invariants.sql + scenarios.md.

Aturan yang bikin hasilnya akurat:
  - Tulis tiap nama konkret dalam `backtick` — itu yang di-parse jadi kandidat symbol.
  - Nama proc/table Oracle UPPER_SNAKE (USP_*, SPI_*, TORDER). Topic DEV_*/DEV-*.
  - Service: ouchinterface, orderservice, riskmanagementservice, userservice, autoorderservice.
  - Section "Invariant" = seed properti PBT. Ini bagian paling berharga — cuma lo yang tahu.

Bahasa:
  - Nama symbol = persis seperti di code (bukan soal bahasa, itu identitas).
  - Section "Invariant": TULIS DALAM BAHASA INGGRIS, sebagai klausa siap-pakai —
    ini mengalir jadi nama test + komentar di Go PBT & SQL invariant. Contoh:
    "remaining buy limit >= 0 for all valid qty/price", bukan "sisa buy limit ...".
  - Ringkasan / Perubahan perilaku / prosa lain: bahasa bebas, agent paham dua-duanya.
-->

## Ringkasan
Satu-dua kalimat: apa yang berubah dan kenapa.

## Yang disentuh (pakai nama PERSIS dalam `backtick`)
- Proc/function: `USP_...`, `SPI_...`
- Table: `TORDER`
- Topic: `DEV_ORDER_INBOUND`
- Service: `orderservice`

## Perubahan perilaku
- Sebelum: ...
- Sesudah: ...

## Invariants that must hold (English — becomes test names/comments)
- e.g. "remaining buy limit >= 0 for all valid qty/price after the proc runs"
- e.g. "order encode→decode payload roundtrip is lossless"

## Di luar scope
- ...
