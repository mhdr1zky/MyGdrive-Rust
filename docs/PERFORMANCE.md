# Kinerja dan batas pengukuran

Versi 1.0.1 mengurangi kerja dan waktu tunggu pemeriksaan sinkronisasi:

- Profil baru memakai interval cloud 10 detik. Interval tersimpan tetap dipakai.
- Jadwal sukses dihitung dari awal siklus. Siklus panjang diselesaikan dahulu,
  lalu menunggu minimal 500 ms. Kegagalan tetap memakai backoff setelah selesai.
- Watcher menggabungkan perubahan lokal sekitar 500 ms, lalu membangunkan
  scheduler tanpa menunggu polling. Transfer aktif tetap diselesaikan.
- Maksimal empat GET metadata rutin berjalan bersamaan. Hasil siap diproses
  lebih dahulu, dengan validasi ulang snapshot setelah kunci file diperoleh.
  Perubahan cloud/filesystem dan transfer isi background tetap berurutan.
- Scan identik melewati transaksi per-file, pencocokan direktori memakai indeks
  inode/path, dan ledger direktori kosong tidak memuat seluruh inventaris.
- Daftar offline memakai koneksi read-only terpisah dari writer dan kontrol;
  snapshot transaksi WAL tetap konsisten.
- File lokal yang sudah dihapus sebelum unggah pertama diselesaikan tanpa
  meminta penghapusan cloud. Jurnal aktif dan review tidak dilewati.

## Pengukuran terkontrol

Fixture tiruan memakai 24 file yang sudah diunduh dan latensi buatan 40 ms
untuk setiap GET metadata selain root, dengan profil test tanpa optimasi.

| Ukuran | Sebelum | Sesudah |
| --- | ---: | ---: |
| Waktu siklus | 1.150 ms | 360 ms |
| GET metadata file bersamaan, maksimum | 1 | 4 |
| Total GET termasuk root | 25 | 25 |
| Unduhan ulang isi | 0 | 0 |

Fixture ini sekitar 3,2 kali lebih cepat. Ini bukan benchmark Google Drive
nyata atau jaminan throughput internet. Konkurensi metadata tidak berarti
empat transfer isi background sekaligus.

## Batas dan validasi

Inventaris awal, proyeksi namespace, dan graph offline pada akun besar masih
memerlukan CPU/RAM. Pemeriksaan cloud memakai polling. Tidak ada preemption
transfer aktif, resume unduhan Range, atau jaminan seluruh sinkronisasi selesai
dalam 10 detik. Ukuran file, jaringan, API, dan konflik memengaruhi durasinya.

Regresi mencakup prioritas edit lokal, snapshot yang berubah selama prefetch,
akses yang hilang, batas konkurensi, respons create yang hilang, restart,
unpin pada file terbuka, crash recovery, dan pin subfolder mandiri. Pengujian
FUSE nyata tersedia ketika lingkungan menyediakan `/dev/fuse`; tes OAuth live
tetap opt-in agar tes otomatis tidak mengubah Google Drive pengguna.
