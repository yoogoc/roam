# Icon attribution

`gpui-component` names its icons by relative path (`icons/arrow-left.svg`) but does
not ship the files, so they are vendored here. The names are dictated by
`gpui_component::IconName` — 86 of them in 0.5.1.

- **75 icons — [lucide](https://lucide.dev), ISC License.** Fetched unmodified
  from `lucide-icons/lucide`. Copyright (c) for portions of Lucide are held by
  Cole Bemis 2013-2022 as part of Feather (MIT); all other copyright (c) for
  Lucide are held by Lucide Contributors 2022.

- **1 icon — `github.svg`, [Simple Icons](https://simpleicons.org), CC0 1.0
  Universal.** Fetched unmodified from `simple-icons/simple-icons`.

- **10 icons — authored for this repository.** `gpui-component` defines these
  itself and lucide has no equivalent: `close`, `dash`, `inspector`,
  `resize-corner`, `sort-ascending`, `sort-descending`, `window-close`,
  `window-maximize`, `window-minimize`, `window-restore`. Drawn to lucide's
  geometry (24×24, `stroke-width="2"`, round caps and joins) so they sit
  correctly beside the rest.

Only the alpha channel survives rendering — gpui's svg renderer converts the
rasterised pixmap into an alpha mask and the theme supplies the colour — so
`stroke="currentColor"` in these files is inert, not a live colour reference.
