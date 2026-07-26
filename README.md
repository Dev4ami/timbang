# Timbang

Alat debat antar-AI untuk keperluan pribadi. Dua model berdebat pro–kontra atas
satu klaim, dimoderasi model ketiga, lalu klaim faktual mereka dicek. Hasilnya
peta argumen — bukan keputusan, bukan pemenang.

Kenapa tidak ada pemenang: skor kemenangan terasa seperti bukti padahal isinya
cuma penumpukan bias framing dan selisih kekuatan model. Begitu ada angka,
pengguna berhenti bertanya "argumen mana yang belum dijawab" dan mulai bertanya
"siapa yang menang". Baca [`CLAUDE.md`](CLAUDE.md) untuk prinsip lengkap.

## Yang bisa dilihat

- **Dua kolom berseberangan** — Pro kiri, Kontra kanan. Ketimpangan panjang
  turn kelihatan tanpa baca satu kata.
- **Chip serangan** — tiap turn menandai klaim lawan yang dia serang. Turn
  tanpa chip = bicara sendiri, sinyal kelembekan.
- **Panel status klaim** — argumen yang tidak pernah dijawab siapa pun
  dipimpin, ditandai paling menonjol. Kebalikan dari kebiasaan UI karena di
  sini yang penting bukan yang menang tapi yang tidak dijawab.
- **Badge fact-check** — klaim faktual ditandai terdukung/diragukan/tak-bisa
  diverifikasi. "Diragukan" satu-satunya yang menarik mata — itu yang perlu
  kamu cek sendiri.
- **Panel sintesis tertutup** — dibuka manual. Kalau ringkasan muncul di atas,
  seluruh transkrip jadi hiasan.
- **Tidak ada kolom input di halaman sesi** — kamu hanya mengetik saat framing.

## Yang tidak akan pernah ada

Skor kemenangan, jumlah "Pro unggul N dari M sesi", dashboard statistik, kolom
chat di tengah debat, sintesis otomatis. Lihat §10 di [`CLAUDE.md`](CLAUDE.md).

## Model

- **Pro & Kontra** — wajib dari lab berbeda dengan tier setara (§10). Default:
  `cc/claude-opus-4-7` vs `kr/deepseek-3.2`. Model `owned_by=combo` **dilarang**
  untuk Pro/Kontra karena provider chained bisa dijawab model lain tanpa
  ketahuan, dan transkrip akan bilang "Pro = X" padahal Y yang menulis.
- **Moderator** — ekstrak klaim, framing. Default `kr/claude-haiku-4.5`.
- **Synthesizer** — belum dijalankan (sengaja, §1).
- **Fact-checker** — klasifikasi faktual/opini + verdict. Default
  `cc/claude-opus-4-7`.

## Menjalankan lokal

Butuh Rust stable + `ROUTER_API_KEY` di `.env`:

```sh
cp .env.example .env
# isi ROUTER_API_KEY
cargo run --bin web
# buka http://127.0.0.1:7878
```

Atau CLI untuk uji prompt cepat:

```sh
cargo run --bin uji jalan "topik yang mau diperdebatkan"
cargo run --bin uji fact-check <id_sesi>   # jalankan ulang fact-check
cargo run --bin uji sehat                  # tes koneksi router
cargo run --bin uji model                  # daftar model di router
```

Baca help lengkap: `cargo run --bin uji`

## Deploy (Coolify + Cloudflare Access)

Dockerfile sudah disiapkan untuk deploy container. Bind `0.0.0.0` di dalam
container **hanya aman** kalau public path punya auth di depan — Coolify di
jaringan private, Traefik → Cloudflare Tunnel → Cloudflare Access (login
Google/email OTP). Baca §6 di [`CLAUDE.md`](CLAUDE.md).

Coolify setup:

1. **Application** → build pack Dockerfile → repo Git ini
2. **Environment (secret)**: `ROUTER_API_KEY=<key-router-mu>`
3. **Persistent storage**: mount volume ke `/app/sesi` supaya sesi bertahan
   restart
4. **Ports**: expose 7878
5. **Domain**: sambung ke Cloudflare tunnel
6. Cloudflare → **Zero Trust → Access → Application** → policy `email is
   <email-mu>`

Env var yang dikenali app:

| Env var | Wajib | Fungsi |
|---|---|---|
| `ROUTER_API_KEY` | ya | API key 9router |
| `TIMBANG_BIND` | (deploy) | timpa `bind` di `config.toml` runtime |
| `TIMBANG_ALLOW_PUBLIC_BIND` | (deploy) | `=1` untuk terima bind non-loopback |

Dockerfile default set dua env terakhir ke `0.0.0.0:7878` dan `1`. `cargo run`
biasa di laptop tidak terpengaruh — env absent, bind tetap loopback.

## Bahasa

Kode & komentar: **Inggris**. Field JSON & config: **Inggris**. Teks yang
dilihat pengguna: **Indonesia**. Prompt di `prompts/*.md` bahasa **Indonesia**.

## Lisensi

Untuk pakai pribadi. Kalau model Claude di router ditarik dari langganan Claude
Code, pemakaian untuk aplikasi sendiri di luar Claude Code berada di luar
peruntukan langganan itu — pakai API key berbayar.
