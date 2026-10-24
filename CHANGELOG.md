# Catatan rilis

## v1.0.0 — 24 Oktober 2026

Rilis pertama **MyGdrive Rust** untuk Linux, dengan CLI, dashboard GTK4, tray,
dan mount FUSE opsional. Paket `.deb` mencakup ketiga binary:
`mygdrive-sync`, `mygdrive-desktop`, dan `mygdrive-mount`.

### Fitur

- Sinkronisasi Google Drive dua arah dengan watcher lokal, polling remote,
  SQLite WAL, antrean persisten, serta pemulihan operasi setelah restart.
- Penyimpanan default hanya-online, unduhan saat dibuka melalui FUSE, dan
  pilihan tersedia offline untuk file atau folder secara rekursif.
- Konflik konten mempertahankan kedua versi. Pemulihan tulisan editor pada
  inode lama membutuhkan peninjauan sebelum diunggah.
- Unggah, rename, move, dan Trash memakai pemeriksaan versi server; perubahan
  bersamaan ditahan untuk rekonsiliasi atau peninjauan.
- Login OAuth melalui browser dengan PKCE, profil akun/folder terpisah,
  serta kontrol pause/resume, sync now, dan shutdown melalui Unix socket.
- Dashboard aktivitas, tray, autostart per profil, serta integrasi Thunar.
- Cache konten dengan checksum dan pembersihan LRU yang melindungi file
  offline, perubahan lokal, dan operasi aktif.

### Batas versi ini

- Ditujukan untuk My Drive dan file biasa. Google Workspace native belum
  mendukung sinkronisasi isi; Shared Drives belum didukung.
- Satu pasangan akun/folder per state; supervisor banyak profil belum tersedia.
- Salinan lokal belum dapat didehidrasi otomatis. Unduhan terputus diulang
  dari awal, dan token OAuth belum dienkripsi menggunakan keyring OS.
- Paket Linux dibangun pada Ubuntu 24.04 untuk arsitektur amd64 dan membutuhkan
  runtime GTK4 serta FUSE3. Panduan instalasi, desktop, offline, dan mount
  tersedia di README; penyiapan akun dijelaskan dalam OAUTH_SETUP.md.
