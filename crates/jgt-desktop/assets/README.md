# Material Symbols Outlined (Subset)

This directory contains a static, subsetted TrueType font (`material-symbols-outlined.ttf`)
derived from Google's [Material Symbols](https://github.com/google/material-design-icons) font under the **Apache License 2.0** (see `LICENSE-material-symbols.txt`).

## Font Details
- **Source**: `variablefont/MaterialSymbolsOutlined[FILL,GRAD,opsz,wght].ttf` from `google/material-design-icons`
- **Instance**: FILL=0, GRAD=0, opsz=24, wght=400 (pinned via `fontTools.varLib.instancer`)
- **Family Name**: `MaterialSymbolsOutlined` / `Material Symbols Outlined`
- **File size**: ~6.6 KB (subsetted to ~33 required glyphs in Unicode Private Use Area)

## Production Commands
The subset was produced in an isolated temporary environment outside the repository:

```bash
# 1. Set up throwaway virtualenv and install fonttools
python3 -I -m venv /tmp/jgt25-fonts/venv
/tmp/jgt25-fonts/venv/bin/pip install fonttools

# 2. Download variable font and codepoints from upstream repository
mkdir -p /tmp/jgt25-fonts/download
curl -sSL "https://github.com/google/material-design-icons/raw/master/variablefont/MaterialSymbolsOutlined%5BFILL%2CGRAD%2Copsz%2Cwght%5D.ttf" \
  -o /tmp/jgt25-fonts/download/MaterialSymbolsOutlined_variable.ttf
curl -sSL "https://raw.githubusercontent.com/google/material-design-icons/master/variablefont/MaterialSymbolsOutlined%5BFILL%2CGRAD%2Copsz%2Cwght%5D.codepoints" \
  -o /tmp/jgt25-fonts/download/MaterialSymbolsOutlined.codepoints
curl -sSL "https://raw.githubusercontent.com/google/material-design-icons/master/LICENSE" \
  -o crates/jgt-desktop/assets/LICENSE-material-symbols.txt

# 3. Instantiate static font with axes pinned to FILL=0, GRAD=0, opsz=24, wght=400
/tmp/jgt25-fonts/venv/bin/python3 -I -c "
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer

font = TTFont('/tmp/jgt25-fonts/download/MaterialSymbolsOutlined_variable.ttf')
instancer.instantiateVariableFont(font, {'FILL': 0, 'GRAD': 0, 'opsz': 24, 'wght': 400}, inplace=True)
font.save('/tmp/jgt25-fonts/MaterialSymbolsOutlined-Static.ttf')
"

# 4. Subset for required glyph unicodes
/tmp/jgt25-fonts/venv/bin/pyftsubset \
  /tmp/jgt25-fonts/MaterialSymbolsOutlined-Static.ttf \
  --output-file=crates/jgt-desktop/assets/material-symbols-outlined.ttf \
  --unicodes="U+E023,U+E0C9,U+E145,U+E149,U+E15F,U+E1A1,U+E2C7,U+E2C8,U+E2E6,U+E5CC,U+E5CD,U+E5CF,U+E5D7,U+E5D8,U+E5DB,U+E627,U+E668,U+E871,U+E8AF,U+E8B8,U+E91F,U+E97A,U+E9F4,U+EA0B,U+EA4A,U+EB7F,U+EF63,U+EF7A,U+F01F,U+F06C,U+F083,U+F0BE,U+F233" \
  --layout-features='*'

# 5. Verify cmap in subset font
/tmp/jgt25-fonts/venv/bin/python3 -I -c "
from fontTools.ttLib import TTFont
font = TTFont('crates/jgt-desktop/assets/material-symbols-outlined.ttf')
cmap = font.getBestCmap()
assert len(cmap) == 33
"
```

## Glyphs Included
| Icon | Code Point | Material Symbol Name |
| --- | --- | --- |
| Board | `U+EB7F` | `view_kanban` |
| Dashboard (alt Board) | `U+E871` | `dashboard` |
| Coordinator | `U+E9F4` | `hub` |
| Psychology (alt Coord) | `U+EA4A` | `psychology` |
| Worker | `U+F06C` | `smart_toy` |
| Groups (alt Worker) | `U+F233` | `groups` |
| Hierarchy | `U+E97A` | `account_tree` |
| Archive | `U+E1A1` | `inventory_2` |
| Archive (alt) | `U+E149` | `archive` |
| Settings | `U+E8B8` | `settings` |
| Search | `U+EF7A` | `search` |
| Hearing | `U+E023` | `hearing` |
| Headphones | `U+F01F` | `headphones` |
| Listen | `U+E91F` | `record_voice_over` |
| Chat | `U+E0C9` | `chat` |
| Reply All | `U+E15F` | `reply_all` |
| Replies | `U+E8AF` | `forum` |
| Expand More | `U+E5CF` | `expand_more` |
| Chevron Right | `U+E5CC` | `chevron_right` |
| Add | `U+E145` | `add` |
| Close | `U+E5CD` | `close` |
| Unfold More | `U+E5D7` | `unfold_more` |
| Arrow Upward | `U+E5D8` | `arrow_upward` |
| Arrow Downward | `U+E5DB` | `arrow_downward` |
| Cost | `U+EF63` | `payments` |
| Working | `U+E627` | `sync` |
| Bolt (alt Working) | `U+EA0B` | `bolt` |
| Check | `U+E668` | `check` |
| Check Circle | `U+F0BE` | `check_circle` |
| Task Alt | `U+E2E6` | `task_alt` |
| Warning | `U+F083` | `warning` |
| Folder | `U+E2C7` | `folder` |
| Folder Open | `U+E2C8` | `folder_open` |

# File-type icons (`file-icons/`)

The colored 16x16 SVGs in `file-icons/` (file types and open/closed folders, used by the IDE
file picker and tabs) were drawn for this project and are our own work, under the project's
license; no third-party icons or license apply. Their style is inspired by, not copied from,
the VS Code Material Icon Theme. They are embedded with `include_bytes!` and rasterized once
with resvg (`src/file_icons.rs`); keep them free of `<text>` elements (resvg is built
without text support).
