# slopshot 📸🤖

> A tiny photo culler for Sony (`.ARW`) and Canon (`.CR3`) RAW files. Open folder, star, pick, reject, export. That's it.

---

## ⚠️⚠️⚠️ HUGE DISCLAIMER ⚠️⚠️⚠️

```
 ██████████████████████████████████████████████████████████████
 █                                                            █
 █   THIS ENTIRE APP IS VIBECODED.                            █
 █                                                            █
 █   Every line was written by an AI. I typed English.        █
 █   The AI typed Rust. Nobody read the borrow checker        █
 █   errors except the AI, and it fixed them by vibes.        █
 █                                                            █
 █   I made this because I couldn't find a simple ARW         █
 █   culler that didn't want a subscription, a catalog,       █
 █   or my firstborn. So I prompted one into existence.       █
 █                                                            █
 █   NO SUPPORT. NO WARRANTY. NO ROADMAP. NO SLA.             █
 █   NO PROMISES IT WON'T EAT YOUR PHOTOS (it shouldn't,      █
 █   it only copies, but hey — back up your stuff).           █
 █                                                            █
 ██████████████████████████████████████████████████████████████
```

```
        ,-~~-.___.
       / |  '     \          "I have no idea what I'm doing"
      (  )         0              — the dev, about this codebase
       \_/-, ,----'
          ====           //
         /  \-'~;    /~~~(O)
        /  __/~|   /       |
      =(  _____| (_________|
```

**FAQ**

| Question | Answer |
|---|---|
| Does it work? | 🔥🐶☕ *"This is fine."* It works on my machine™ (Sony a6400 and a6700, Linux). |
| Will you fix my bug? | lol. lmao, even. |
| Can I open an issue? | You can. I may read it. An AI may fix it. Neither is guaranteed. |
| PRs welcome? | Sure, but you're reviewing AI slop with more AI slop. Godspeed. |
| Is the code good? | It has comments. It compiles. It passed the vibe check. |
| Why is it called slopshot? | Because it's AI slop that shoots (photos). Read the room. |
| Should I use this for my wedding clients' photos? | Back them up first. Then maybe. Then back them up again. |

> **tl;dr:** 100% AI-generated, 0% support. If it helps you, nice. If it breaks, you get to keep both pieces.

---

## Features

Despite all of the above, it actually does stuff:

### Culling
- **Open a folder** of `.ARW` / `.CR3` files (native folder picker, or pass the folder as an argument)
- **Gallery grid** with thumbnails, filename, capture date and an EXIF summary (`50mm  f/2.8  1/125s  ISO 1000`)
- **Star ratings** (1–5), **pick** ✔ and **reject** ✖ flags. Rejected shots get dimmed so they stop judging you.
- **Ratings persist** in a small `.slopshot.json` file in the photo folder. Your RAWs are never modified.
- **In-camera ratings** are imported if you starred shots on the camera
- **Export** copies every picked file to a folder of your choice, with a progress bar. Existing files are skipped, never overwritten.

### Filter & sort
- Filter by **name**, **minimum rating**, **flag** (picked / rejected / unflagged / not rejected) and **date range**. Partial dates work (`2026-10` = all of October).
- Sort by **name**, **date** or **rating**, ascending or descending

### Viewing
- **Large view** with **zoom** (scroll, `+`/`-`, pinch) and **pan** (drag)
- **Full resolution when zoomed**, from the best available source:
  1. the camera JPG next to the RAW (RAW+JPEG shooters)
  2. the full-size JPEG embedded in the RAW (a6700-generation Sony bodies, every CR3)
  3. a real RAW develop via [`rawler`](https://crates.io/crates/rawler) if neither exists
- **Focus point overlay**: shows where the camera focused (eye/face box on supported Sony bodies; the face box or in-focus AF points on Canon). `Z` zooms to 100% *right on the eye*, and stepping through a burst while zoomed recentres on each shot's focus point. Great for "which one is actually sharp".
- **EXIF info panel**: camera, lens, focal length (and full-frame equivalent), aperture, shutter, ISO, exposure comp, resolution, file size
- **Film strip** 🎞️ under the large view: the current photo with 3–6 neighbours each side (as many as fit), in the grid's filter/sort order, with flags and stars. Click a frame to jump to it, `S` to hide it. It's drawn from the thumbnails, so it doesn't care what brand your camera is. It does have sprocket holes, because a model that has never seen film decided film needs sprocket holes.
- **Display brightness**: an exposure slider plus **Auto** brightness, for those night shots where the preview is basically a black rectangle. Display only; files are never touched.

### Speed
- Never decodes the RAW just to browse. It reads the JPEG previews the camera already embedded.
- Thumbnails decode in the background on half your cores at low priority, so your machine stays usable
- Neighbouring photos are pre-decoded, so arrow-keying through the large view is instant
- On a6700-class bodies and CR3s the full-res JPEG is fetched once you pause on a photo, so zooming feels instant
- Parsing is a hand-rolled TIFF/EXIF/MakerNote reader that needs ~2 reads per ARW (3–4 per CR3, which hides its metadata a box or two deeper). Nice on slow USB drives. An earlier version of this README said "~2 reads per file" for CR3 too. A reviewer counted. The AI has been informed.

---

## Keyboard shortcuts

| Key | Action |
|---|---|
| `←` `→` `↑` `↓` | Move selection / previous–next photo |
| `1`–`5` | Rate · `0` clears |
| `P` / `X` / `U` | Pick / reject / unflag · `P`/`X` again clears it |
| `Enter` / `Space` / double-click | Open large view |
| `Esc` | Back to grid |
| `Z` | 100% at the focus point / back to fit |
| Double-click (large view) | 100% where you clicked / back to fit |
| Scroll, `+`, `-` | Zoom |
| Drag | Pan |
| `A` | Toggle auto brightness |
| `F` | Toggle focus point overlay |
| `I` | Toggle info panel |
| `S` | Toggle film strip (large view) |

---

## Building

You need a recent Rust toolchain (built with 1.91).

```sh
git clone https://github.com/the-rectifier/slopshot.git
cd slopshot
cargo build --release
./target/release/slopshot                     # opens with a folder picker
./target/release/slopshot ~/Pictures/shoot    # or open a folder directly
```

On Linux the folder picker goes through the XDG desktop portal (`xdg-desktop-portal` plus a backend for your desktop). Windows works: a contributor ran it on Windows 11, so for once the README was *underselling* itself. macOS *should* work since every dependency is cross-platform. That has never been tested. See disclaimer. 🙃

The first build takes a while because `rawler` is chunky. Go make coffee. ☕

---

## Tested on

- Sony **a6400** (RAW+JPEG, landscape night shots)
- Sony **a6700** (RAW only, portrait cosplay shots)
- Linux (Wayland, Hyprland)

Contributor-tested (the maintainer has never touched one):

- Canon **EOS RP** (CR3, EF 24-105, beach shots in Chania, Windows 11). CR3 support was contributed by a *different* AI, prompted by a *different* human, and reviewed by yet another AI. The slop is now multi-vendor. 🤖🤝🤖

Other Sony bodies will probably work, since they share the ARW format. Other CR3 bodies (EOS R/M/90D and newer) share one layout too, so they *should* work. "Should" is doing a lot of heavy lifting there. NEF/RAF/CR2 aren't opened at all. If you want those, you know what to do (prompt it yourself 🤖).

Before you blame the AI, check what slopshot actually reads from your files (preview size, EXIF, rating, focus point):

```sh
SLOPSHOT_SAMPLES=~/Pictures/shoot cargo test --release samples -- --ignored --nocapture
```

---

## Known limitations

- Only Sony `.ARW` and Canon `.CR3` files
- The RAW-develop fallback has no camera tone curve, so it looks flatter than the camera JPEG. It's brightness-matched to the preview, but it isn't a raw editor.
- Mirrored orientations (EXIF 2/4/5/7) aren't handled. Your camera almost certainly never writes them.
- One full-res image is kept in memory at a time (~100–200 MB for 24–26 MP)

---

## License

[MIT](LICENSE). Do whatever you want with it. If it breaks, you were warned in a box made of block characters (and in the license, in all caps).

```
  ┌──────────────────────────────────────────────┐
  │  slopshot: proudly mass-produced by a model  │
  │  that has never held a camera                │
  └──────────────────────────────────────────────┘
```
