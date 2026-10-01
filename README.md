# Span

*Span* is a provisional name; the repository is `crosspane`.

Span makes several computers act as one workspace:

- **One keyboard and mouse** move across machines, as if all displays were attached to one computer.
- **Individual application windows are projected** onto another machine. The app keeps running on its
  own machine, and the window disappears from its source while it is shown elsewhere.

Machines connect peer to peer over any IP link (LAN, direct Ethernet, USB4/Thunderbolt networking),
are explicitly paired, and encrypt everything.

## Status

**Phase 0: foundations and risk spikes.** The plan was approved on 2026-09-30. The MVP targets an
Apple-silicon Mac (macOS 26+) and Linux on Hyprland, in both directions. Windows follows in Phase 3.

- The plan: [`docs/plan/README.md`](docs/plan/README.md)
- Work-package tracker: [`docs/wp/README.md`](docs/wp/README.md)
- Spike reports: [`docs/spikes/README.md`](docs/spikes/README.md)

## Licence

GPL-3.0-or-later. See [`LICENSE`](LICENSE).
