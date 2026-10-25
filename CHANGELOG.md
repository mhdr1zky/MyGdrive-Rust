# Catatan rilis

## v1.0.4 — 25 Oktober 2026

- Tampilan dashboard, aktivitas sinkronisasi, akun/folder, dan pilihan offline
  dirapikan dengan gaya konsisten, teks yang membungkus, dan tombol yang menyesuaikan
  lebar jendela. Aktivitas sinkronisasi dapat dibuka langsung dari dashboard.
- Dashboard menampilkan versi aplikasi yang sedang berjalan, status rilis GitHub,
  pemeriksaan manual, dan tautan catatan rilis.
- Pemeriksaan berkala tidak lagi memasang pembaruan otomatis. Unduhan dan pemasangan
  paket hanya dimulai setelah pengguna memilih **Update sekarang**. Build source
  membuka halaman rilis untuk pemasangan manual.

## v1.0.3 — 25 Oktober 2026

- Pembaruan desktop otomatis dari GitHub Releases untuk `.deb` amd64 dan
  AppImage x86_64: pemeriksaan enam jam, unduhan terverifikasi SHA-256,
  penggantian AppImage atomik, dan pemasangan `.deb` melalui autentikasi
  administrator Linux. Status tampil di sidebar; versi baru dipakai setelah
  membuka ulang aplikasi/service. Build source dan mode snapshot dikecualikan.

## v1.0.2 — 25 Oktober 2026

- Kegagalan izin saat membaca metadata satu file tidak lagi menghentikan
  pemrosesan file lain. Penyebabnya tersimpan per file dan dicoba kembali;
  kegagalan akun, koneksi, dan pembatasan permintaan tetap menghentikan siklus.
- Pembangunan ulang inventaris menandai item lama yang tidak ditemukan sebagai
  tidak tersedia setelah seluruh daftar selesai. Daftar parsial dan restart
  tidak menghilangkan namespace lama atau perubahan lokal yang belum diunggah.
- Salinan outbox unggahan normal yang sudah selesai dibersihkan secara otomatis.
  Antrean aktif, operasi dibatalkan, konflik, berkas perlu tinjauan, serta blob
  tanpa jurnal tetap dilindungi. Riwayat operasi selesai tetap tersedia.
- Aktivitas sinkronisasi menampilkan penyebab kegagalan dan tindakan pemulihan
  untuk izin file, kuota, login, koneksi, batas layanan, dan penyimpanan lokal.

- Skrip publikasi GitHub satu perintah dengan pratinjau, penyaringan berkas
  privat/hasil build, checkout terpisah, dan perlindungan konflik. Tes skrip
  serta launcher dijalankan otomatis melalui GitHub Actions.

## v1.0.1 — 25 Oktober 2026

- Paket AppImage x86_64 untuk dashboard, sinkronisasi folder lokal, pilihan
  offline, dan CLI/helper mount FUSE. GTK dibundel; integrasi Thunar tetap
  memerlukan setup native. Autostart menyimpan lokasi AppImage yang tetap.
- Pin file/subfolder mandiri, turunan folder termasuk anak baru dan direktori
  kosong, serta informasi eksplisit ketika layanan sedang dijeda.
- Lock Office/LibreOffice dan scratch Word yang dikenali tidak masuk antrean
  baru; penyimpanan atomik melalui file sementara lalu rename tetap didukung.

- File lokal yang dihapus sebelum unggahan pertama kini selesai secara lokal,
  termasuk lock editor lama yang sudah hilang. Antrean tidak lagi menunggu
  penghapusan cloud untuk file tanpa identitas cloud. Jurnal aktif dan file
  pemulihan tetap dilindungi; respons create yang hilang dipulihkan sebelum
  penghapusan cloud dijalankan.
- Pemeriksaan metadata rutin file bersih memakai maksimal empat GET bersamaan;
  hasil siap diproses lebih dahulu, sementara perubahan dan transfer isi
  background tetap berurutan. Edit lokal tetap didahulukan dan snapshot yang
  berubah dibaca ulang. Fixture 24 file dengan latensi metadata buatan 40 ms
  mencatat 1.150 → 360 ms dengan jumlah GET yang sama, tanpa unduhan ulang;
  hasil ini bukan benchmark koneksi Google Drive nyata.
- Polling bawaan profil baru menjadi 10 detik, dengan jadwal sukses dihitung
  dari awal siklus dan jeda minimal 500 ms setelah siklus panjang. Interval
  tersimpan dipertahankan; kegagalan tetap memakai backoff penuh.
- Scan file identik melewati transaksi per-file; pencocokan direktori memakai
  indeks inode/path dan ledger direktori kosong melewati pembacaan inventaris
  remote. Launcher source memilih set lengkap executable yang lebih baru dari
  release/debug, tanpa mencampur profil desktop/service/mount.
- Daftar tersedia offline memakai koneksi SQLite read-only persisten khusus
  yang terpisah dari writer dan pembaca kontrol/status, dengan snapshot
  transaksi WAL tetap konsisten selama sinkronisasi.
- Panel aktivitas sinkronisasi kini terbuka melalui **Lihat detail** pada
  notifikasi atau tindakan manual dari tray/menu, tanpa muncul otomatis saat
  operasi baru terdeteksi.
- Melepas pilihan offline kini mengantrekan pembersihan otomatis pada
  sinkronisasi berikutnya yang berhasil saat service tidak dijeda dan online.
  Salinan lokal file bersih yang sudah tersinkron serta cache terkait dibuang;
  perubahan tertunda, file perlu tinjauan, file terbuka, dan pin yang masih
  berlaku tetap dilindungi sampai aman. Metadata tetap terlihat melalui mount
  online, dan membuka file kembali mengunduhnya lagi. File cloud tidak dihapus.

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
