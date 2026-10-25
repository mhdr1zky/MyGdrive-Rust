# MyGdrive Rust

Sinkronisasi Google Drive untuk Linux, ditulis dengan Rust. Akses My Drive dari desktop dan pengelola berkas, unduh konten saat dibutuhkan, lalu pilih file atau folder yang ingin tetap tersedia tanpa internet.

**Rilis terbaru: [v1.0.1](https://github.com/mhdr1zky/MyGdrive-Rust/releases/tag/v1.0.1)** — paket `.deb` amd64 untuk Ubuntu 24.04/Linux Mint 22 dan AppImage x86_64. Proyek ini independen dan tidak berafiliasi dengan Google.

Versi ini memperbaiki antrean file baru yang sudah dihapus sebelum unggah, pilihan offline mandiri/rekursif, pelepasan ruang disk setelah unpin, serta waktu pemeriksaan sinkronisasi. Lihat [catatan rilis](CHANGELOG.md).

## Preview aplikasi

Tangkapan layar aplikasi di Linux, menggunakan profil dan berkas contoh. Tampilan drive virtual di Thunar berasal dari paket native; lihat perbedaan paket di bawah.

| Dashboard sinkronisasi | Drive online di Thunar |
| --- | --- |
| ![Dashboard MyGdrive Rust](docs/screenshots/dashboard.png) | ![Drive virtual MyGdrive di Thunar](docs/screenshots/online-drive.png) |
| **Pilihan tersedia offline** | **Detail aktivitas sinkronisasi** |
| ![Pilihan file dan folder tersedia offline](docs/screenshots/offline-browser.png) | ![Jendela detail aktivitas sinkronisasi](docs/screenshots/sync-activity.png) |

## Fitur

- **Sinkronisasi dua arah:** unggah, unduh, rename, pindahkan, dan Trash file atau folder dengan pemeriksaan perubahan server.
- **Hanya-online secara default:** inventaris menyimpan metadata; konten diunduh ketika dibuka melalui mount online atau dipilih tersedia offline.
- **Pilihan offline per file/folder:** pilihan folder berlaku secara rekursif, termasuk anak baru.
- **File sementara editor:** file pemilik Office `~$…`, lock LibreOffice `.~lock.…#`, dan pola scratch Word yang dikenal tidak masuk antrean sinkronisasi baru. Penyimpanan dokumen melalui file sementara lalu rename tetap didukung.
- **Konflik mempertahankan kedua versi:** perubahan isi yang berbeda disimpan sebagai salinan konflik. Pemulihan tertentu ditahan untuk ditinjau sebelum diunggah.
- **Dashboard dan tray:** lihat status, progres, antrean, serta aktivitas; jeda/lanjutkan sinkronisasi dan jalankan sinkronisasi sekarang.
- **Aktivitas tanpa popup otomatis:** notifikasi tetap muncul; jendela aktivitas dibuka melalui tombol **Lihat detail** atau tindakan manual pengguna.
- **Profil terpisah:** simpan pasangan akun Google, folder Drive, dan folder lokal dengan state masing-masing.
- **Integrasi Linux:** mount FUSE, menu konteks dan badge folder Thunar, serta autostart per profil yang dapat diaktifkan pengguna.
- **Antrean dan cache persisten:** SQLite WAL, checksum konten, pemulihan operasi setelah restart, dan pembersihan cache yang melindungi file offline serta perubahan lokal.
- **File sementara yang sudah hilang selesai otomatis:** file lokal tanpa identitas cloud tidak terus menunggu penghapusan; jurnal unggahan aktif dan file pemulihan tetap diperiksa sebelum antrean diselesaikan.

## Pilih paket

| Fitur | `.deb` | AppImage |
| --- | --- | --- |
| Dashboard, tray, login OAuth, sinkronisasi folder lokal | Ya | Ya |
| Pilih file/subfolder tersedia offline dan unpin | Ya | Ya |
| Helper drive virtual FUSE | Ya | Ya, melalui `--mount` |
| Integrasi menu dan badge Thunar | Setup native | Setup manual; gunakan `.deb` untuk integrasi lengkap |
| Mulai saat login | Opsional | Opsional; lokasi AppImage harus tetap |

## Menjalankan AppImage

Unduh `mygdrive-rust_1.0.1_x86_64.AppImage` dari [rilis v1.0.1](https://github.com/mhdr1zky/MyGdrive-Rust/releases/tag/v1.0.1), lalu jalankan:

```bash
chmod +x mygdrive-rust_1.0.1_x86_64.AppImage
./mygdrive-rust_1.0.1_x86_64.AppImage
# CLI dan helper drive virtual:
./mygdrive-rust_1.0.1_x86_64.AppImage --sync --help
./mygdrive-rust_1.0.1_x86_64.AppImage --mount --help
```

AppImage x86_64 ditargetkan untuk **Ubuntu 24.04/Linux Mint 22 atau sistem dengan glibc 2.39+**. Pustaka GTK disertakan; driver grafis, font, browser, dan layanan desktop memakai sistem host. Ini bukan sandbox. Data/token memakai lokasi native yang sama (`~/.local/share/mygdrive/`); jangan jalankan `.deb` dan AppImage bersamaan pada profil yang sama.

Runtime AppImage memakai FUSE; jika mount AppImage tidak didukung, jalankan `./mygdrive-rust_1.0.1_x86_64.AppImage --appimage-extract-and-run`. Drive virtual aplikasi tetap membutuhkan dukungan FUSE host dan izin `/dev/fuse` tersendiri. AppImage tidak memasang menu konteks/badge Thunar secara otomatis dan tidak memulai mount online secara otomatis. Untuk mount manual, gunakan `--mount --state /path/profil --mount /path/mount`; siapkan direktori mount kosong. Gunakan `.deb` untuk launcher mount dan integrasi Thunar terkelola.

Simpan AppImage pada lokasi tetap sebelum mengaktifkan **Mulai saat login**. Untuk memperbarui, tutup aplikasi dan layanan profil, lalu ganti file dengan AppImage versi baru pada lokasi yang sama. Lihat [panduan penggunaan AppImage](https://docs.appimage.org/user-guide/run-appimages.html).

## Instalasi `.deb`

Unduh `mygdrive-rust_1.0.1_amd64.deb` dari bagian **Releases** repositori, lalu jalankan dari direktori unduhan:

```bash
sudo apt install ./mygdrive-rust_1.0.1_amd64.deb
```

`apt` memasang dependensi runtime yang diperlukan. Rust dan compiler tidak diperlukan untuk menjalankan paket. Rilis ini menyediakan paket amd64; ARM, Windows, macOS, dan Debian 12 belum menjadi target paket v1.

Untuk memperbarui v1.0.0, keluar melalui menu **Keluar** agar operasi aktif selesai, lalu instal `.deb` baru. Folder lokal, akun, dan pilihan offline tetap digunakan. Metadata state bermigrasi dari schema 3 ke 4; v1.0.0 tidak dapat membaca state yang sudah dimigrasi. Simpan cadangan state lama bila perlu kembali ke versi sebelumnya.

Buka **MyGdrive Rust** dari menu aplikasi atau terminal:

```bash
/usr/bin/mygdrive-desktop
```

Jika sebelumnya memasang launcher dari source, entri per pengguna di `~/.local/share/applications/io.mygdrive.Desktop.desktop` dapat masih menunjuk checkout lama dan mendahului launcher paket. Arsipkan entri lama tersebut melalui pengelola berkas jika ingin menu memakai versi `.deb`. Perintah `/usr/bin/mygdrive-desktop` selalu membuka versi paket. Sesuaikan direktori tersebut jika `$XDG_DATA_HOME` diatur.

| Perintah | Fungsi |
| --- | --- |
| `mygdrive-desktop` | Dashboard GTK4 dan tray |
| `mygdrive-sync` | Login, sinkronisasi, status, dan kontrol service |
| `mygdrive-mount` | Mount online FUSE untuk profil yang sudah disiapkan |

## Mulai menggunakan

1. Siapkan OAuth client Google bertipe **Desktop app** dan unduh JSON-nya. Ikuti [panduan OAuth](OAUTH_SETUP.md); kredensial bersama tidak disertakan dalam paket.
2. Buka **MyGdrive Rust → Akun dan folder → Hubungkan akun baru…**, pilih JSON client, lalu selesaikan login melalui browser sistem.
3. Pilih **My Drive** atau subfolder yang ingin disinkronkan, lalu pilih folder lokal kosong yang terpisah dari direktori state aplikasi.
4. Simpan pasangan folder dan buka dashboard profil tersebut. Mulai service dari dashboard untuk membaca inventaris dan menjalankan sinkronisasi.
5. Gunakan menu **Tersedia offline** untuk memilih konten yang perlu disimpan di komputer. File atau subfolder dapat dipilih sendiri meskipun folder induk dan My Drive hanya online. Memilih folder berlaku untuk seluruh turunannya, termasuk anak baru. Tunggu unduhan selesai sebelum menggunakannya tanpa internet.

Jika pilihan sudah aktif tetapi file belum muncul secara lokal, periksa bahwa service profil yang sama sedang berjalan, tidak dijeda, dan terhubung. Saat dijeda, pilihan tetap tersimpan; tekan **Lanjutkan** pada dashboard untuk menjalankan unduhan. Jendela offline menampilkan kondisi dijeda secara eksplisit. Memilih folder induk bukan syarat untuk mengunduh satu file atau subfolder.

Melepas pilihan offline mengantrekan pembersihan otomatis pada sinkronisasi berikutnya yang berhasil saat service berjalan, tidak dijeda, dan online. File bersih yang sudah tersinkron serta cache terkait dibuang dari disk; perubahan tertunda, file yang sedang dibuka, dan pin yang masih berlaku tetap dilindungi sampai aman. File di Google Drive tidak dihapus. Metadata tetap terlihat melalui mount online, dan membuka file kembali akan mengunduhnya lagi. Untuk melihat item hanya-online di pengelola berkas, siapkan mount FUSE berikut.

### Mount online dan Thunar

Profil dashboard tersimpan di `~/.local/share/mygdrive/profiles/ID_PROFIL/`. Untuk menemukan profil yang benar, cocokkan nilai `root` di `service-config.json` dalam direktori tersebut dengan folder lokal pilihan Anda. Contoh berikut menggunakan placeholder `ID_PROFIL`; ganti dengan ID direktori profil Anda dan gunakan label akun yang sesuai konfigurasinya. Jika `$XDG_DATA_HOME` diatur, sesuaikan direktori data pada contoh:

```bash
python3 /usr/lib/mygdrive-rust/scripts/configure-online-drive.py \
  --state "$HOME/.local/share/mygdrive/profiles/ID_PROFIL" \
  --account default \
  --mount "$HOME/MyGDrive-Online"

mygdrive-desktop --state "$HOME/.local/share/mygdrive/profiles/ID_PROFIL"
```

Mount harus kosong, dimiliki pengguna, dan berada di luar folder lokal maupun direktori state. Launcher desktop memulai mount yang sudah dikonfigurasi. Service profil juga harus berjalan agar konten dapat diunduh saat dibuka.

Untuk menambahkan menu **Selalu tersedia offline** dan **Lepaskan pilihan offline** di Thunar, jalankan sebagai pengguna biasa:

```bash
python3 /usr/lib/mygdrive-rust/scripts/install-thunar-actions.py \
  --state "$HOME/.local/share/mygdrive/profiles/ID_PROFIL" \
  --account default \
  --mount "$HOME/MyGDrive-Online"
```

Installer mempertahankan aksi Thunar lain dan membuat cadangan konfigurasi sebelumnya. Integrasi ini opsional. Pilihan **Mulai saat login** pada dashboard mengaktifkan autostart untuk profil yang sedang dibuka.

## Menggunakan CLI

Contoh berikut membuat pasangan CLI tersendiri. `DRIVE_FOLDER_ID` adalah ID folder Google Drive; gunakan `root` untuk My Drive. Gunakan nilai `--state`, `--account`, `--root`, dan `--drive-folder` yang sama untuk pasangan ini.

```bash
mkdir -p "$HOME/MyGDrive"

# Login melalui browser; JSON client bukan access token.
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" --account default \
  login --client-json "$HOME/Downloads/oauth-client.json" --scope drive

# Sinkronisasi satu kali.
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" --account default \
  --root "$HOME/MyGDrive" --drive-folder DRIVE_FOLDER_ID sync

# Watcher lokal dan polling Drive berkelanjutan.
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" --account default \
  --root "$HOME/MyGDrive" --drive-folder DRIVE_FOLDER_ID run --interval 10
```

Profil baru memakai polling cloud 10 detik; interval profil yang sudah tersimpan tetap dipertahankan bila tidak diberikan override. Watcher lokal membangunkan sinkronisasi setelah perubahan, tanpa menunggu polling. Pemeriksaan metadata rutin file bersih dapat memakai empat GET bersamaan; transfer isi background tetap berurutan.

Kontrol service dari terminal lain:

```bash
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service status
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service pause
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service resume
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service sync-now
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service shutdown
```

`scan` dan `status` dapat digunakan tanpa mengakses Google Drive. Perintah CLI yang menulis state seperti `scan`, `sync`, `pin`, dan `cache-prune` membutuhkan lock yang sama dengan daemon; hentikan daemon terlebih dahulu. Gunakan kontrol service di atas saat daemon aktif. Lihat opsi lengkap dengan `mygdrive-sync --help`.

Uji tiruan 24 file dengan latensi metadata 40 ms mencatat waktu 1.150 → 360 ms, sekitar 3,2 kali lebih cepat, dengan jumlah GET yang sama dan tanpa unduhan ulang. Angka ini bukan jaminan kecepatan Google Drive nyata. Kondisi pengukuran dan batasnya ada di [catatan kinerja](docs/PERFORMANCE.md).

## Data lokal dan akses akun

State default berada di `~/.local/share/mygdrive` atau mengikuti `$XDG_DATA_HOME`. Profil dari dashboard disimpan pada subdirektori `profiles/`. State memuat konfigurasi, SQLite, cache, antrean upload, dan token OAuth; folder lokal pengguna disimpan terpisah.

Login menggunakan browser sistem, PKCE, pemeriksaan state, dan callback loopback. Token OAuth disimpan dengan izin privat (`0700` untuk direktori dan `0600` untuk file), tetapi belum dienkripsi melalui keyring OS. Simpan JSON OAuth dan direktori state secara privat.

Scope `drive` memberi akses baca dan kelola pada seluruh file Drive yang dapat diakses akun. Memilih subfolder hanya membatasi perilaku aplikasi, bukan izin OAuth. Scope `drive-file` lebih terbatas dan tidak otomatis memberi akses ke file lama; integrasi Google Picker belum tersedia. Lihat [penjelasan scope dari Google](https://developers.google.com/workspace/drive/api/guides/api-specific-auth).

## Batas v1.0.1

- Mendukung **My Drive dan file biasa** seperti dokumen Office, PDF, gambar, teks, dan arsip. Isi Google Docs/Sheets/Slides native belum mendukung sinkronisasi dan round-trip.
- Shared Drives dan seluruh tampilan **Shared with me** belum didukung.
- Setiap state terikat pada satu pasangan akun, folder lokal, dan folder Drive. Profil dapat dibuka terpisah; supervisor semua profil belum tersedia.
- Pembersihan setelah pelepasan pin menunggu sinkronisasi berhasil. File dengan perubahan tertunda, perlu tinjauan, sedang dibuka, atau masih dipin sendiri maupun melalui induk tetap disimpan.
- Unduhan yang terputus dimulai ulang; resume Range belum tersedia.
- Symlink, hardlink, path non-UTF-8, dan jenis file khusus ditolak oleh scanner.
- Konflik metadata, delete-versus-edit, dan pemulihan tertentu membutuhkan peninjauan. Trash folder menggunakan pemeriksaan dan kompensasi, bukan transaksi atomik di Google Drive.
- Inventaris awal akun besar dapat memerlukan waktu. Dukungan tray dan badge bergantung pada sesi desktop serta pengelola berkas.

## Perbandingan dengan Google Drive for desktop

Perbandingan ini diperiksa pada **25 Oktober 2026** dari dokumentasi resmi Google. MyGdrive Rust menyediakan klien Linux; aplikasi desktop resmi Google ditujukan untuk Windows dan macOS. Tabel ini memisahkan perilaku yang sudah tersedia dari pekerjaan yang masih diperlukan, tanpa menganggap seluruh fitur Google Drive sudah setara.

| Perilaku | Google Drive for desktop | MyGdrive Rust saat ini |
| --- | --- | --- |
| Daftar file tanpa mengunduh seluruh isi Drive | Mode streaming memakai drive virtual; isi diambil ketika diperlukan. [Streaming dan mirroring](https://support.google.com/drive/answer/13401938?hl=en). | Default metadata saja; mount FUSE menyediakan nama file/folder dan mengunduh file saat dibuka. Setup mount masih memerlukan konfigurasi tersendiri. |
| Pilihan offline per file/subfolder | File atau folder dapat dipilih sendiri; pilihan folder juga mencakup isi dan item baru. [Akses offline](https://support.google.com/drive/answer/2375012?hl=en). | Pilihan mandiri dan rekursif, termasuk turunan baru; tidak perlu mempin induk. Unduhan menunggu service berjalan dan tidak dijeda. |
| Ruang disk untuk konten streaming | Cache menyimpan konten yang dipakai dan dikelola otomatis; tersedia tampilan Offline files untuk melihat pemakaian. [Pengaturan cache](https://support.google.com/drive/answer/13470231?hl=en). | Unpin mengantrekan pelepasan salinan bersih dan cache terkait. Budget cache opsional belum menjadi pengelolaan otomatis seluruh salinan hasil membuka file; ringkasan ruang disk belum tersedia. |
| Mode mirror My Drive | Pengguna dapat beralih antara streaming dan mirror salinan penuh. [Mode sinkronisasi](https://support.google.com/drive/answer/13401938?hl=en). | Alur pengguna berfokus pada hanya-online dan pin. Mode mirror internal belum memiliki pengaturan dashboard dan migrasi mode yang didukung pengguna. |
| Shared Drives | Shared Drives memakai streaming. [Jenis drive](https://support.google.com/drive/answer/13401938?hl=en). | Baru My Drive/subfolder; Shared Drives dan tampilan Shared with me belum didukung. |
| Beberapa akun dalam satu aplikasi | Mendukung sampai empat akun bersamaan. [Pengaturan akun](https://support.google.com/drive/answer/13470231?hl=en). | Profil akun/folder terpisah dapat dijalankan; belum ada supervisor dan tampilan aktivitas gabungan seluruh profil. |
| Google Docs, Sheets, Slides offline | Akses offline disiapkan melalui Drive di web. [Dokumen Google offline](https://support.google.com/drive/answer/2375012?hl=en). | Format native belum bisa diedit offline atau round-trip; file biasa seperti DOCX/XLSX/PPTX/PDF dapat diunduh dan disinkronkan. |

Google menyatakan dukungan backup Google Photos melalui Drive for desktop berakhir **10 Agustus 2026** pada [halaman pengaturan resmi](https://support.google.com/drive/answer/13470231?hl=en). Fitur lama tersebut tidak dijadikan target kesetaraan klien ini.

## Rencana pengembangan

Daftar ini adalah urutan pekerjaan yang diusulkan, bukan fitur yang sudah tersedia atau janji tanggal rilis.

1. **Keandalan pilihan offline dan informasi kegagalan.** Pertahankan pengujian file/subfolder mandiri melalui dashboard, IPC, dan Thunar; turunan lama/baru mengikuti pin induk, termasuk direktori kosong. Tampilkan sebab unduhan tertunda, koneksi, izin unduh, dan kesiapan seluruh isi folder dengan jelas. Lanjutkan uji pemulihan saat aplikasi berhenti di tengah unduhan atau pelepasan salinan lokal. Perluas deteksi siklus simpan Excel untuk file sementara bernama acak tanpa mengabaikan file pengguna yang bernama serupa. File `.tmp`, backup, serta file lock yang sudah tercatat sebelum filter diperkenalkan tetap dipertahankan; jangan membersihkannya tanpa pemeriksaan terpisah.
2. **Pengelolaan ruang disk menyeluruh.** Tambahkan ringkasan ukuran pin/cache/antrean, budget bawaan yang dapat diatur, dan pelepasan otomatis salinan bersih yang hanya pernah dibuka. Koleksi snapshot upload yang sudah selesai harus tetap melindungi operasi aktif dan berkas pemulihan; pembersihan cache saat ini belum mencakup seluruh area state.
3. **Setup desktop dan beberapa profil.** Satukan konfigurasi mount/menu Thunar ke onboarding, periksa binary/service yang benar setelah pembaruan, lalu sediakan supervisor seluruh profil dengan aktivitas dan kegagalan per akun.
4. **Mode dan cakupan Drive.** Sediakan perpindahan streaming/mirror yang memverifikasi seluruh perubahan lokal sebelum melepas salinan; kemudian tambahkan Shared Drives serta identitas/izin yang diperlukan untuk item bersama dan shortcut.
5. **Alur dokumen Google dan transfer besar.** Tambahkan pembukaan native Docs/Sheets/Slides di web dan ekspor yang jelas dibedakan dari sinkronisasi format native. Lanjutkan dengan resume unduhan serta pengaturan transfer; ekspor tidak boleh dipresentasikan sebagai editor offline native Google.

## Build dari source

Gunakan Rust stable **1.92 atau lebih baru**, Python 3.11+, dan dependensi build Linux. Pada Ubuntu 24.04:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev dpkg-dev python3

# Build ketiga binary dan buat paket .deb.
./scripts/build-deb.sh

# Hasil build lokal.
sudo apt install ./dist/mygdrive-rust_1.0.1_amd64.deb
```

AppImage memakai binary release dan `Cargo.lock` yang sama:

```bash
./scripts/build-appimage.sh
# Jika build release sudah selesai:
./scripts/build-appimage.sh --skip-build
```

Script mematok versi dan SHA-256 appimagetool/runtime resmi, membundel dependensi GTK beserta catatan lisensinya, memeriksa ketiga executable, lalu menghasilkan satu file AppImage. Dependensi build tambahan: `curl`, `mksquashfs` (paket `squashfs-tools`), `glib-compile-schemas`, serta tool GdkPixbuf. Build dilakukan pada Ubuntu 24.04; glibc dan driver grafis tidak dibundel.

Build CLI saja tidak membutuhkan GTK:

```bash
cargo build --locked --release --bin mygdrive-sync
```

Launcher source memilih set lengkap desktop/service/mount yang lebih baru antara `target/release` dan `target/debug`, dengan release sebagai pilihan saat waktunya sama. Ketiganya harus executable agar dianggap satu set lengkap. Paket `.deb` dibangun memakai profil release.

Pemeriksaan pengembangan:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features --all-targets
```

Tes otomatis memakai adapter tiruan dan server lokal. Tes live onboarding tetap opt-in. Untuk rilis, samakan versi di `Cargo.toml`, `Cargo.lock`, catatan rilis, dan `.github/release-version`, lalu push ke `main`; workflow menguji, membangun `.deb` dan AppImage, mengunggah checksum, kemudian memublikasikan rilis beserta tag pada commit tersebut. Push tag versi juga didukung. Detail perubahan tersedia pada [catatan rilis](CHANGELOG.md).

## Lisensi

[MIT](LICENSE). Nama Google Drive dan merek terkait merupakan milik pemegang masing-masing.
