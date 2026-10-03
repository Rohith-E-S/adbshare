#!/usr/bin/env python3
"""Install the Lucide icon set into adbshare's assets.

The GUI used to draw its own 16x16 icons. Those are replaced by Lucide, a
maintained set drawn on a 24x24 grid with a 2px stroke and rounded caps, which
is what `gpui::svg` expects and what makes the chrome look designed rather than
drafted.

The mapping is semantic-name -> Lucide-name, so call sites keep reading as
intent (`names::FOLDER_DOWNLOAD`) while the asset is the real Lucide file. Run
from the crate directory:

    python3 tools/fetch_icons.py <path-to-lucide-checkout>

The generator that drew the previous set, gen_icons.py, is gone: these are
vendored, not synthesised, so they need refreshing rather than regenerating.
"""

import shutil
import sys
from pathlib import Path

OUT = Path("assets/icons")

# adbshare name -> Lucide name. Where Lucide has no folder variant for a
# concept, the content glyph stands in: GNOME and Zed do the same, and a plain
# folder for all six places told the user nothing.
MAPPING = {
    "application-x-executable": "terminal",
    "audio-x-generic": "audio-lines",
    "camera-photo": "camera",
    "checkbox-checked": "check",
    "dialog-error": "circle-alert",
    "dialog-information": "info",
    "dialog-warning": "triangle-alert",
    "document-edit": "file-pen",
    "document-open": "external-link",
    "document-send": "send",
    "drive-harddisk": "hard-drive",
    "edit-copy": "copy",
    "edit-delete": "trash",
    "edit-find": "search",
    "edit-paste": "clipboard-paste",
    "edit-select-all": "square-check-big",
    "edit-undo": "undo-2",
    "emblem-symbolic-link": "link",
    "emblem-synchronizing": "refresh-cw",
    "folder-copy": "copy",
    "folder-new": "folder-plus",
    "folder-upload": "folder-up",
    "go-down": "chevron-down",
    "go-down-bold": "arrow-down",
    "go-next": "chevron-right",
    "go-previous": "chevron-left",
    "go-up": "chevron-up",
    "help-about": "circle-question-mark",
    "image-x-generic": "file-image",
    "media-playback-pause": "pause",
    "media-playback-start": "play",
    "network-wireless": "wifi",
    "object-select": "check",
    "package-x-generic": "package",
    "phone": "smartphone",
    "process-stop": "square",
    # `send-to` was also mapped from `folder-send`; one name for one glyph.
    "send-to": "send-horizontal",
    "sidebar-show": "panel-left",
    "system-file-manager": "folder-search",
    "system-lock": "lock",
    "system-software-install": "download",
    "text-x-generic": "file-text",
    "user-home": "house",
    "user-trash": "trash",
    "utilities-terminal": "terminal",
    "video-x-generic": "video",
    "view-continuous": "activity",
    "view-grid": "grid-2x2",
    "view-list": "rows-3",
    "view-more": "ellipsis-vertical",
    "view-refresh": "refresh-cw",
    "window-close": "x",
    "x-office-document": "file",
}


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    src = Path(sys.argv[1]) / "icons"
    if not src.is_dir():
        print(f"no icons directory at {src}", file=sys.stderr)
        return 1

    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)

    # Lucide emits `width`/`height` alongside the viewBox. Dropping them lets
    # GPUI size the element, which is what `icon()` does.
    missing = []
    for adb_name, lucide_name in sorted(MAPPING.items()):
        source = src / f"{lucide_name}.svg"
        if not source.is_file():
            missing.append(lucide_name)
            continue
        text = source.read_text()
        for attribute in ('  width="24"\n', '  height="24"\n'):
            text = text.replace(attribute, "")
        (OUT / f"{adb_name}.svg").write_text(text)

    # The ISC notice has to travel with the files; Lucide's own LICENSE also
    # records which icons derive from Feather.
    shutil.copy(Path(sys.argv[1]) / "LICENSE", OUT / "LICENSE")

    print(f"installed {len(MAPPING) - len(missing)} icons into {OUT}")
    if missing:
        print("missing upstream: " + ", ".join(missing), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
