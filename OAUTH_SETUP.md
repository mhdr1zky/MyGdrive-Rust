# Menyiapkan OAuth untuk MyGdrive Rust

Setiap pengguna menyiapkan OAuth client sendiri. Repo dan paket `.deb` tidak menyertakan JSON client, token akun, atau konfigurasi proyek Google Cloud milik pengembang.

## 1. Aktifkan Google Drive API

Buka [Google Cloud Console](https://console.cloud.google.com/), buat atau pilih proyek Anda, lalu aktifkan **Google Drive API** melalui **APIs & Services → Library**.

## 2. Siapkan layar persetujuan

Atur nama aplikasi, email dukungan, audience, dan test user melalui **Google Auth Platform**. Untuk penggunaan pribadi dengan audience **External** berstatus **Testing**, tambahkan akun yang akan digunakan sebagai test user. Nama menu Console dapat berubah; ikuti [panduan konfigurasi persetujuan Google](https://developers.google.com/workspace/guides/configure-oauth-consent).

Tambahkan scope Drive yang sesuai. MyGdrive Rust meminta scope secara eksplisit ketika login:

| Opsi CLI | Scope | Akses |
| --- | --- | --- |
| `--scope drive` | `https://www.googleapis.com/auth/drive` | Membaca dan mengelola file Drive yang dapat diakses akun, termasuk file yang sudah ada |
| `--scope drive-file` | `https://www.googleapis.com/auth/drive.file` | File yang dibuat atau diotorisasi untuk aplikasi; tidak otomatis mencakup file lama |

Pemilihan subfolder dalam aplikasi tidak mempersempit izin OAuth. Sinkronisasi file lama umumnya memerlukan `drive`; alur Google Picker untuk mengotorisasi file satu per satu belum tersedia. Lihat [daftar dan klasifikasi scope resmi](https://developers.google.com/workspace/drive/api/guides/api-specific-auth).

## 3. Buat client Desktop app

Di bagian **Clients**, buat OAuth client dengan jenis **Desktop app**, lalu unduh JSON-nya. File harus memiliki bagian `installed`, bukan `web`. Simpan di lokasi privat, misalnya:

```bash
chmod 600 "$HOME/Downloads/oauth-client.json"
```

Client ID adalah pengenal aplikasi, bukan token akun. JSON client juga tidak dapat digunakan sebagai `access.token`. Alur aplikasi menggunakan browser sistem dan callback loopback pada port lokal acak, sesuai [OAuth untuk aplikasi desktop](https://developers.google.com/identity/protocols/oauth2/native-app).

## 4. Login

Pada dashboard, buka **Akun dan folder → Hubungkan akun baru…**, pilih JSON client, lalu selesaikan persetujuan di browser.

Untuk CLI:

```bash
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" --account default \
  login --client-json "$HOME/Downloads/oauth-client.json" --scope drive
```

Gunakan state dan label akun yang sama saat menjalankan sinkronisasi. Token disimpan privat pada `STATE/oauth/` dan direfresh otomatis. Token belum dienkripsi dengan keyring OS.

Jika callback kedaluwarsa atau browser menampilkan koneksi ditolak, mulai login baru. URL callback lama tidak dapat dipakai kembali. CLI menyediakan `--timeout-seconds 1800` untuk memberi waktu 30 menit dan `--no-browser` untuk membuka URL secara manual di komputer yang sama.

## 5. Batas penggunaan

Menurut [kebijakan kedaluwarsa token Google](https://developers.google.com/identity/protocols/oauth2#expiration), refresh token untuk proyek External berstatus Testing dengan scope Drive umumnya berlaku tujuh hari. Login ulang mungkin diperlukan. Distribusi publik perlu memenuhi ketentuan publishing dan verifikasi Google yang berlaku untuk scope aplikasi.

Jangan unggah JSON client, authorization code, access token, refresh token, atau direktori state ke repositori. `.gitignore` proyek mengecualikan nama file kredensial umum; tetap periksa isi commit sebelum publikasi.
