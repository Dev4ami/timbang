# Timbang

Alat debat antar-AI untuk keperluan pribadi. Dua model berdebat pro–kontra atas satu klaim,
dimoderasi model ketiga, lalu hasilnya disintesis jadi **peta argumen** — bukan keputusan.

Pengguna: satu orang. Jalan di localhost. Bukan produk, tidak ada pengguna lain.
Bahasa kode & komentar: Inggris. Bahasa output ke pengguna: Indonesia.

---

## 1. Prinsip yang tidak boleh dilanggar

Bagian ini lebih penting daripada bagian mana pun di file ini. Kalau ada usulan perubahan
yang bertentangan dengan salah satu poin di bawah, tolak dan tanyakan dulu ke pengguna.

### Tidak ada pemenang

Sistem ini **tidak boleh** menentukan sisi mana yang menang, tidak boleh memberi skor,
tidak boleh menampilkan voting, dan tidak boleh menghitung statistik "Pro unggul N dari M sesi".

Alasannya: pengguna memakai alat ini untuk berpikir sendiri. Angka kemenangan terasa seperti
bukti padahal isinya cuma penumpukan dua hal yang tidak berhubungan dengan kebenaran — bias
cara klaim dirumuskan, dan selisih kekuatan antar model. Begitu ada skor, pengguna berhenti
bertanya "argumen mana yang belum dijawab" dan mulai bertanya "siapa yang menang". Itu
membuat cara bacanya lebih dangkal.

Output akhir berbentuk kondisional: "kalau yang kamu prioritaskan A → arah X lebih masuk akal;
kalau B → arah Y", plus daftar hal yang perlu dicek sendiri.

### Konsensus antar-AI bukan bukti

Jangan pernah menyajikan kesepakatan antar model sebagai penguat. Semua model dilatih dari
korpus yang mirip, jadi mereka bisa salah bersamaan.

### Pro dan Kontra harus benar-benar berseberangan

Masalah utama sistem debat multi-agent adalah model bawaannya sopan dan mudah kompromi.
Ronde 3 mulai "poin Anda valid, tapi…", ronde 5 sudah sepakat, debat mati. Semua mekanisme
anti-konvergensi di file ini ada untuk mencegah itu, dan tidak boleh dilonggarkan demi
kerapian output.

### Sintesis terkunci sampai debat selesai

Panel sintesis & crux ditutup sampai seluruh fase selesai, dan pengguna harus membukanya
manual. Kalau ringkasan muncul di atas, pengguna hanya membaca itu dan seluruh transkrip
jadi hiasan.

### Tidak ada kolom input di halaman sesi

Satu-satunya tempat pengguna mengetik adalah halaman framing, sebelum debat mulai. Kalau
pengguna bisa menyela di tengah debat, kedua model akan menangkap arah maunya dan melunak
ke sana — dan seluruh guna alat ini hilang.

### Prompt tidak boleh berada di dalam kode

Semua prompt hidup di `prompts/*.md`. Yang paling sering diubah pengguna adalah prompt,
bukan logika. Kalau prompt jadi string literal di `.rs`, setiap eksperimen kecil kena compile
dan momentum eksplorasi mati.

---

## 2. Arsitektur

Satu library, dua binary. Semua logika ada di library; binary hanya lapisan tipis.

```
src/lib.rs          inti debat
src/bin/web.rs      produk yang dipakai sehari-hari
src/bin/uji.rs      alat kerja permanen: jalankan 1 fase, cetak hasil ke stdout
```

`bin/uji` bukan versi lama yang ditinggal. Dia tetap dipakai setiap kali prompt diubah,
karena untuk menguji apakah rebuttal melunak tidak butuh browser atau streaming — butuh
melihat teks cepat, sepuluh kali berturut-turut.

### Peta modul

| Modul | Isi | Catatan |
|---|---|---|
| `config` | model per role, jumlah ronde, batas kata, path prompt | baca-tulis saat runtime |
| `llm` | satu fungsi: kirim teks → terima teks | **sengaja bodoh.** Tidak tahu apa pun soal debat |
| `transcript` | `Turn`, `Claim`, simpan/muat, checkpoint | file adalah sumber kebenaran |
| `view` | view builder | **jantung sistem.** Lihat §4 |
| `phase` | state machine + syarat lolos tiap fase | `enum` + `match` |
| `render` | transkrip → markdown / HTML | |

`llm` bodoh dan `view` pintar. Kalau terbalik, kodenya jadi kusut.

---

## 3. Model data

```
Turn   { ronde, fase, role, model_diminta, model_menjawab, teks, waktu, token }
Claim  { id, pemilik, isi, status: Hidup | Terbantah | Diabaikan, ronde_lahir }
Sesi   { id, klaim, status, config_terpakai, turns, claims, penilaian }
```

`Claim` dilacak sebagai entitas terpisah, bukan teks lepas. Tanpa itu sistem tidak bisa
menjawab pertanyaan paling berguna dari seluruh proyek: **argumen mana yang dilempar tapi
tidak pernah dijawab siapa pun.** Itu biasanya titik buta bersama kedua model — dan
kemungkinan besar titik buta penggunanya juga.

`model_menjawab` diambil dari **respons**, bukan dari request. Lihat §5 soal combo.

`config_terpakai` disimpan di dalam file sesi, bukan hanya di config global. Pengguna akan
bereksperimen menukar model; kalau setting hanya global, sesi lama tidak bisa dibandingkan
atau direproduksi.

`penilaian` — satu field, tiga nilai: crux kepakai / setengah / tidak. Diisi manual oleh
pengguna setelah sintesis. Satu ketukan, tanpa kolom komentar, tanpa skor angka. Ini satu-satunya
data yang tidak bisa direkam ulang belakangan, jadi harus ada sejak tahap paling awal.

---

## 4. Alur fase & view builder

```
Framing → Opening → Rebuttal → Cross-exam → Crux → Sintesis → Selesai
```

Tiap fase mendefinisikan empat hal: siapa yang jalan, urutannya, view-nya apa, dan syarat lolos.

### Siapa melihat apa

| Fase | Pro melihat | Kontra melihat |
|---|---|---|
| Opening | kosong | kosong |
| Rebuttal | opening Kontra | opening Pro + rebuttal Pro |
| Cross-exam | pertanyaan Kontra saja | — |
| Crux | seluruh transkrip | seluruh transkrip |

Di Opening keduanya buta agar argumen awalnya independen, tidak langsung menempel pada
framing lawan. Dua request ini boleh paralel.

**Giliran siapa yang jalan lebih dulu harus ditukar setiap ronde.** Yang jalan belakangan
selalu untung karena melihat lebih banyak.

Kalau view salah, prompt sebagus apa pun tidak menolong. Kalau view benar, prompt sederhana
pun jalan. Compiler harus dipaksa menangani setiap kombinasi fase × role — bug "agent melihat
sesuatu yang seharusnya tidak dia lihat" tidak menyebabkan crash, dia cuma membuat debat
lembek tanpa ketahuan sebabnya.

### Syarat lolos

Contoh untuk Rebuttal: harus menyebut minimal satu klaim lawan, dan kemiripan dengan turn
sendiri di ronde sebelumnya di bawah ambang tertentu. Gagal → moderator menyuntik teguran,
minta ulang sekali. Gagal lagi → catat di transkrip sebagai "gagal membantah". Itu informasi,
bukan error.

### Framing wajib disetujui pengguna

Moderator mengajukan 2–3 rumusan klaim beserta keterangan bias masing-masing. Pengguna
memilih atau menulis sendiri, baru debat jalan. Alasannya: cara klaim dirumuskan menentukan
siapa yang menang. Kalau moderator merumuskan sendiri tanpa dilihat, sebagian keputusan sudah
diambil sebelum debat dimulai. Ini bug epistemik, tidak bisa diperbaiki di kode.

Konsekuensi: sesi punya status yang hidup di disk (`menunggu_persetujuan` → `berjalan` → `selesai`),
dan checkpoint ditulis **per fase**, bukan hanya di akhir.

---

## 5. Integrasi 9router

Endpoint OpenAI-compatible, self-hosted, base URL disimpan di `config.toml`.

- `POST {base}/v1/chat/completions` — body `{model, messages, temperature, max_tokens}`
- Header `Authorization: Bearer {key}`
- Header **`X-9Router-Token-Saver: off`** — wajib. Router punya kompresi otomatis yang
  menyasar output tool; kemungkinan tidak menyentuh transkrip debat, tapi "kemungkinan" tidak
  cukup. Kalau argumen terkompresi diam-diam, waktu habis untuk men-debug prompt yang tidak salah.
- `GET {base}/api/health` → `{"ok":true}` — dipakai tombol tes koneksi
- `GET {base}/v1/models` → `data[].id` untuk mengisi dropdown model. Format id berprefix
  penyedia, contoh `openai/gpt-5`

### Dilarang memakai combo untuk Pro dan Kontra

Model bertanda `owned_by: "combo"` adalah beberapa provider dirantai dengan fallback otomatis.
Bagus untuk coding, merusak di sini: kalau provider utama kena limit, turn bisa dijawab model
lain tanpa ketahuan. Transkrip akan bilang "Pro = X" padahal yang menulis Y, lalu pengguna
membandingkan sesi dan menyimpulkan dari data yang bohong.

Pro dan Kontra wajib model eksplisit `provider/model`. Moderator boleh combo — dampaknya kecil.

### Taksonomi error

| Error | Perlakuan |
|---|---|
| `401` | gagal cepat, jangan retry. Masalah config, bukan jaringan |
| `400 Invalid model format` | gagal cepat, tampilkan di halaman setting |
| `503 All accounts unavailable` | retry pakai header `retry-after` |
| timeout / jaringan | retry backoff, 3×, lalu berhenti dengan status checkpoint |

Retry karena jaringan dan retry karena output model jelek adalah dua logika berbeda. Jangan
digabung di satu tempat.

Router lewat tunnel adalah titik gagal tunggal. Kalau request gagal di ronde 4 dari 6, sesi
berhenti dengan status gagal dan transkrip sampai situ aman — pengguna melanjutkan, tidak
memulai ulang.

---

## 6. Konfigurasi & keamanan

```
.env             ROUTER_API_KEY                          ← .gitignore
.env.example     nama variabel tanpa nilai               ← ikut commit
config.toml      base_url, model per role, ronde, path   ← ikut commit
prompts/         pro.md, kontra.md, moderator.md, synthesizer.md
sesi/            {id}.json — checkpoint per fase
```

- `.gitignore` dibuat **sebelum** `.env`. Sekali file terbawa commit, dia hidup di riwayat git selamanya.
- API key dibungkus newtype dengan `Debug` custom yang selalu mencetak `[redacted]`.
  `#[derive(Debug)]` pada struct berisi key akan membocorkannya ke setiap panic dan log.
- Key dibaca sekali di startup, gagal cepat kalau tidak ada. Jangan `unwrap_or_default()`.
- Key hanya boleh disentuh modul `llm`.
- Struct yang dikirim ke browser **terpisah** dari struct `Config`. Jangan pakai struct yang
  sama — kalau field key terbawa karena lupa, dia muncul di DevTools.
- Server bind ke `127.0.0.1`, bukan `0.0.0.0`.
- Browser tidak boleh pernah tahu soal key. Alur: browser → server Rust → 9router → model.

### Yang boleh diubah dari web vs tidak

| Boleh | Tidak boleh |
|---|---|
| model per role, jumlah ronde, batas kata, temperature | API key, base URL router, alamat bind |

Base URL masuk daftar terlarang: kalau halaman web bisa mengubahnya, satu kesalahan bisa
mengarahkan request beserta API key ke server orang lain.

Setting dari web berlaku **per sesi**, bukan global. Halaman setting hanya mengubah default.

---

## 7. Web

Bentuknya **dokumen / transkrip persidangan**, bukan chat. Satu sesi debat disebut satu "sidang".

Chat membuat pengguna merasa jadi peserta, dan chat meratakan struktur — dua sisi, fase, dan
klaim yang saling menyerang jadi tumpukan gelembung seragam. Dari daftar pesan tidak bisa
terlihat argumen mana yang tidak pernah dijawab.

### Halaman

| Rute | Isi |
|---|---|
| `/baru` | topik, konteks fakta opsional, setting model untuk sesi ini |
| `/sesi/{id}/framing` | 2–3 rumusan klaim + keterangan bias, bisa diedit |
| `/sesi/{id}` | halaman utama — lihat di bawah |
| `/riwayat` | daftar sidang, config terpakai, crux, penilaian, kolom diagnostik |
| `/setting` | ubah default + tombol tes koneksi |

Tidak ada halaman dashboard. Bukan karena datanya jelek, tapi karena dashboard mengundang
dibuka setiap hari — dan begitu aplikasi ini dibuka untuk melihat angka, bukan untuk memikirkan
sesuatu, fungsinya sudah berubah.

### Halaman sesi

- Header: klaim + strip fase + status
- Dua kolom berseberangan, Pro kiri, Kontra kanan. Ketimpangan panjang turn jadi terlihat
  tanpa membaca satu kata pun
- Tiap turn punya chip "→ menyerang K5". Turn **tanpa** chip berarti dia bicara sendiri,
  tidak menyambung ke apa pun — itu alat deteksi kelembekan yang terbaca sekilas
- Panel status klaim. Yang berstatus **Diabaikan** ditandai paling menonjol — kebalikan dari
  kebiasaan UI, karena di sini yang penting bukan yang menang tapi yang tidak dijawab
- Panel sintesis & crux tertutup, dibuka manual
- Tidak ada kolom input

### Streaming

Debat makan beberapa menit, jadi tidak boleh satu HTTP request menunggu sampai selesai.

```
POST topik            → balikan id_sesi seketika, kerja jalan di background task
GET /sesi/{id}/stream → SSE, push per turn yang selesai
```

SSE cukup, tidak perlu WebSocket — datanya satu arah. Push **per turn**, bukan per token:
pengguna tidak membaca sambil menunggu.

Karena tata letak dihitung dari `(fase, role, ronde)` dan bukan dari urutan pesan, refresh
browser membangun ulang halaman dari file tanpa logika tambahan.

Frontend: satu file HTML + JS biasa. Tidak perlu framework. Desktop dulu; di layar sempit
kolom turun jadi satu dengan label sisi per turn.

---

## 8. Diagnostik yang boleh ada

Boleh mengukur perilaku sistem. Dilarang mengukur skor debat, dilarang mengukur rajinnya pengguna.

| Boleh | Kenapa |
|---|---|
| tingkat konvergensi per sesi | kalau tinggi, prompt atau kombinasi model bermasalah |
| rasio klaim diabaikan | kalau tinggi di semua sesi, fase rebuttal kurang mengikat |
| retry & gagal format per model | data untuk memilih model |
| panjang turn per ronde | menyusut drastis di ronde akhir = jumlah ronde kelebihan |

| Dilarang | Kenapa |
|---|---|
| Pro menang N dari M | terasa seperti bukti, isinya bias framing + selisih kekuatan model |
| jumlah sesi, streak, jam pemakaian | mengukur kerajinan, bukan ketajaman berpikir |

Semua yang boleh memberi **tindakan** yang jelas. Itu bedanya diagnostik dan hiasan.
Letaknya: kolom kecil di `/riwayat` dan ringkasan di `/setting` — bukan halaman sendiri.

---

## 9. Tahapan

Satu tahap harus **utuh**: bisa dipakai dari awal sampai dapat hasil. Bukan setengah fitur di
semua bagian. Urutannya disusun berdasarkan risiko mana yang paling mahal kalau salah.

| Tahap | Isi | Lulus kalau |
|---|---|---|
| 0 — coretan | 1 file, topik hardcode, 2 request ke 2 model | dapat 2 teks dari 2 model. **Lalu dibuang** |
| 1 — library + `bin/uji` | semua fase, checkpoint, transkrip markdown, penilaian | 10 sesi jalan, prompt sudah tidak lembek |
| 2 — `bin/web` | dua kolom, SSE, framing lewat form, riwayat | satu sesi penuh dari browser |
| 3 — pelacakan klaim | moderator ekstrak klaim, status, chip, diagnostik | panel status klaim akurat |
| 4 — penyempurnaan | fact-checker, dropdown dari `/v1/models`, tes koneksi | — |

Tahap 0 sengaja jelek dan **harus dihapus**, bukan dilanjutkan jadi tahap 1. Kalau dilanjutkan,
keputusan-keputusan asal yang diambil saat masih menebak akan terbawa selamanya.

**Jangan mengerjakan dua tahap sekaligus.** Kalau ada yang tidak jalan, tidak akan ketahuan
apakah itu salah prompt, salah pelacakan klaim, atau salah view builder.

---

## 10. Godaan yang harus ditolak

Semua ini akan terdengar seperti perbaikan. Bukan.

- Menambah skor atau penentuan pemenang "biar ada kesimpulan"
- Menambah kolom chat di halaman sesi "biar bisa tanya lanjutan"
- Membuka sintesis otomatis di atas halaman "biar langsung kelihatan"
- Memakai satu model untuk menulis pro dan kontra sekaligus "biar hemat request"
- Memakai satu `history` bersama untuk semua agent "biar konsisten"
- Memakai model dari satu keluarga yang sama untuk Pro dan Kontra "biar kualitas seragam"
- Memakai dua tier berbeda dari keluarga yang sama, misal Opus versi lama vs baru — sisi yang
  modelnya lebih kuat akan sistematis terlihat lebih meyakinkan, dan debat timpang lebih
  menyesatkan daripada debat lembek
- Membangun dashboard statistik
- Menaruh prompt di dalam kode "biar satu file"
- Meminta model menomori klaimnya sendiri padahal moderator lebih akurat

---

## 11. Belum diputuskan

- Isi prompt tiap role — bagian dengan pengaruh terbesar, belum dibahas
- Model untuk Kontra: harus dari lab berbeda dengan Pro, tier setara. Saat ini router baru
  tersambung ke jalur Claude, jadi satu provider non-Anthropic perlu ditambahkan **sebelum**
  mulai. Ini kerjaan di dashboard router, bukan di kode
- Siapa yang mengekstrak klaim: agent menomori sendiri (murah, rapuh) atau moderator
  mengekstrak (tambah 1 request per turn, akurat). Rekomendasi: moderator, tapi bisa ditunda ke tahap 3
- Format output akhir: markdown yang bisa disimpan, atau hanya tampil di halaman

## Catatan lisensi model

Kalau model Claude di router ditarik dari langganan Claude Code, pemakaian untuk aplikasi
sendiri di luar Claude Code berada di luar peruntukan langganan itu. Untuk pemakaian rutin,
pakai API key berbayar. Biaya per sesi diperkirakan di bawah Rp 8.000.
