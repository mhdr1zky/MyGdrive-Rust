# MyGdrive Rust

Sinkronisasi Google Drive untuk Linux, ditulis dengan Rust. Akses My Drive dari desktop dan pengelola berkas, unduh konten saat dibutuhkan, lalu pilih file atau folder yang ingin tetap tersedia tanpa internet.

**Rilis v1.0.0 menggunakan paket `.deb` untuk amd64 (x86_64), dengan target Ubuntu 24.04 dan Linux Mint 22.** Paket mencakup dashboard GTK4, CLI, tray, dan mount FUSE. Proyek ini independen dan tidak berafiliasi dengan Google.

## Fitur

- **Sinkronisasi dua arah:** unggah, unduh, rename, pindahkan, dan Trash file atau folder dengan pemeriksaan perubahan server.
- **Hanya-online secara default:** inventaris menyimpan metadata; konten diunduh ketika dibuka melalui mount online atau dipilih tersedia offline.
- **Pilihan offline per file/folder:** pilihan folder berlaku secara rekursif, termasuk anak baru.
- **Konflik mempertahankan kedua versi:** perubahan isi yang berbeda disimpan sebagai salinan konflik. Pemulihan tertentu ditahan untuk ditinjau sebelum diunggah.
- **Dashboard dan tray:** lihat status, progres, antrean, serta aktivitas; jeda/lanjutkan sinkronisasi dan jalankan sinkronisasi sekarang.
- **Profil terpisah:** simpan pasangan akun Google, folder Drive, dan folder lokal dengan state masing-masing.
- **Integrasi Linux:** mount FUSE, menu konteks dan badge folder Thunar, serta autostart per profil yang dapat diaktifkan pengguna.
- **Antrean dan cache persisten:** SQLite WAL, checksum konten, pemulihan operasi setelah restart, dan pembersihan cache yang melindungi file offline serta perubahan lokal.

## Instalasi `.deb`

Unduh `mygdrive-rust_1.0.0_amd64.deb` dari bagian **Releases** repositori, lalu jalankan dari direktori unduhan:

```bash
sudo apt install ./mygdrive-rust_1.0.0_amd64.deb
```

`apt` memasang dependensi runtime yang diperlukan. Rust dan compiler tidak diperlukan untuk menjalankan paket. Rilis ini menyediakan paket amd64; ARM, Windows, macOS, dan Debian 12 belum menjadi target paket v1.

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
5. Gunakan menu **Tersedia offline** untuk memilih konten yang perlu disimpan di komputer. Tunggu unduhan selesai sebelum menggunakannya tanpa internet.

Melepas pilihan offline mempertahankan salinan lokal yang sudah ada; tindakan ini belum membebaskan ruang disk. File yang belum diunduh hanya dapat dibuka ketika koneksi tersedia. Untuk melihat item hanya-online di pengelola berkas, siapkan mount FUSE berikut.

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
  --root "$HOME/MyGDrive" --drive-folder DRIVE_FOLDER_ID run --interval 30
```

Kontrol service dari terminal lain:

```bash
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service status
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service pause
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service resume
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service sync-now
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" service shutdown
```

`scan` dan `status` dapat digunakan tanpa mengakses Google Drive. Perintah CLI yang menulis state seperti `scan`, `sync`, `pin`, dan `cache-prune` membutuhkan lock yang sama dengan daemon; hentikan daemon terlebih dahulu. Gunakan kontrol service di atas saat daemon aktif. Lihat opsi lengkap dengan `mygdrive-sync --help`.

## Data lokal dan akses akun

State default berada di `~/.local/share/mygdrive` atau mengikuti `$XDG_DATA_HOME`. Profil dari dashboard disimpan pada subdirektori `profiles/`. State memuat konfigurasi, SQLite, cache, antrean upload, dan token OAuth; folder lokal pengguna disimpan terpisah.

Login menggunakan browser sistem, PKCE, pemeriksaan state, dan callback loopback. Token OAuth disimpan dengan izin privat (`0700` untuk direktori dan `0600` untuk file), tetapi belum dienkripsi melalui keyring OS. Simpan JSON OAuth dan direktori state secara privat.

Scope `drive` memberi akses baca dan kelola pada seluruh file Drive yang dapat diakses akun. Memilih subfolder hanya membatasi perilaku aplikasi, bukan izin OAuth. Scope `drive-file` lebih terbatas dan tidak otomatis memberi akses ke file lama; integrasi Google Picker belum tersedia. Lihat [penjelasan scope dari Google](https://developers.google.com/workspace/drive/api/guides/api-specific-auth).

## Batas v1.0.0

- Mendukung **My Drive dan file biasa** seperti dokumen Office, PDF, gambar, teks, dan arsip. Isi Google Docs/Sheets/Slides native belum mendukung sinkronisasi dan round-trip.
- Shared Drives dan seluruh tampilan **Shared with me** belum didukung.
- Setiap state terikat pada satu pasangan akun, folder lokal, dan folder Drive. Profil dapat dibuka terpisah; supervisor semua profil belum tersedia.
- Pelepasan pin belum menghapus salinan lokal secara otomatis. Budget cache hanya membersihkan objek cache yang memenuhi syarat.
- Unduhan yang terputus dimulai ulang; resume Range belum tersedia.
- Symlink, hardlink, path non-UTF-8, dan jenis file khusus ditolak oleh scanner.
- Konflik metadata, delete-versus-edit, dan pemulihan tertentu membutuhkan peninjauan. Trash folder menggunakan pemeriksaan dan kompensasi, bukan transaksi atomik di Google Drive.
- Inventaris awal akun besar dapat memerlukan waktu. Dukungan tray dan badge bergantung pada sesi desktop serta pengelola berkas.

## Build dari source

Gunakan Rust stable **1.92 atau lebih baru**, Python 3.11+, dan dependensi build Linux. Pada Ubuntu 24.04:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev dpkg-dev python3

# Build ketiga binary dan buat paket .deb.
./scripts/build-deb.sh

# Hasil build lokal.
sudo apt install ./dist/mygdrive-rust_1.0.0_amd64.deb
```

Build CLI saja tidak membutuhkan GTK:

```bash
cargo build --locked --release --bin mygdrive-sync
```

Pemeriksaan pengembangan:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features --all-targets
```

Tes otomatis memakai adapter tiruan dan server lokal. Tes live onboarding tetap opt-in. Workflow CI menjalankan pemeriksaan Rust; push tag yang sesuai versi manifest, seperti `v1.0.0`, menjalankan build dan publikasi paket `.deb` melalui GitHub Actions. Detail perubahan tersedia pada [catatan rilis](CHANGELOG.md).

## Lisensi

[MIT](LICENSE). Nama Google Drive dan merek terkait merupakan milik pemegang masing-masing.
