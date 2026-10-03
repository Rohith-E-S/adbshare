#!/usr/bin/env python3
"""Install Google's Material Symbols into adbshare's assets.

The GUI used to draw its own 16x16 icons, then vendored Lucide. They are now
Material Symbols in the Outlined style, which is what `gpui::svg` expects and
what makes the chrome look designed rather than drafted.

The mapping is semantic-name -> Material-symbol-name, so call sites keep reading
as intent (`names::FOLDER_UPLOAD`) while the asset is the real upstream file.
Run from the crate directory:

    python3 tools/fetch_icons.py

Nothing is vendored from a local checkout: the symbols are fetched from the
upstream repository over HTTPS, so refreshing the set needs nothing but a
network. Pass a directory to install into somewhere other than `assets/icons`,
which is only useful for checking a change before committing it.
"""

import shutil
import subprocess
import sys
from pathlib import Path

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else "assets/icons")

BASE = (
    "https://raw.githubusercontent.com/google/material-design-icons/master"
    "/symbols/web/{name}/materialsymbolsoutlined/{name}_24px.svg"
)

# adbshare name -> Material Symbols name. Where Material has no folder variant
# for a concept, the content glyph stands in: GNOME and Zed do the same, and a
# plain folder for all six places told the user nothing.
MAPPING = {
    "application-x-executable": "terminal",
    "audio-x-generic": "graphic_eq",
    "camera-photo": "photo_camera",
    "checkbox-checked": "check",
    "dialog-error": "error",
    "dialog-information": "info",
    "dialog-warning": "warning",
    "document-edit": "edit_document",
    "document-open": "open_in_new",
    "document-send": "send",
    "drive-harddisk": "hard_drive",
    "edit-copy": "content_copy",
    "edit-delete": "delete",
    "edit-find": "search",
    "edit-paste": "content_paste",
    "edit-select-all": "select_all",
    "edit-undo": "undo",
    "emblem-symbolic-link": "link",
    "emblem-synchronizing": "sync",
    "folder-copy": "content_copy",
    "folder-new": "create_new_folder",
    "folder-upload": "upload",
    "go-down": "keyboard_arrow_down",
    "go-down-bold": "arrow_downward",
    "go-next": "chevron_right",
    "go-previous": "chevron_left",
    "go-up": "keyboard_arrow_up",
    "help-about": "help",
    "image-x-generic": "image",
    "media-playback-pause": "pause",
    "media-playback-start": "play_arrow",
    "network-wireless": "wifi",
    "object-select": "check",
    "package-x-generic": "inventory_2",
    "phone": "smartphone",
    "process-stop": "stop_circle",
    # `send-to` was also mapped from `folder-send`; one name for one glyph.
    "send-to": "send",
    "sidebar-show": "left_panel_open",
    "system-file-manager": "folder_open",
    "system-lock": "lock",
    "system-software-install": "download",
    "text-x-generic": "description",
    "user-home": "home",
    "user-trash": "delete",
    "utilities-terminal": "terminal",
    "video-x-generic": "movie",
    "view-continuous": "monitoring",
    "view-grid": "grid_view",
    "view-list": "view_list",
    "view-more": "more_vert",
    "view-refresh": "refresh",
    "window-close": "close",
    "x-office-document": "draft",
}

# The one attribute that has to go. Material emits `width`/`height` next to the
# viewBox, and GPUI sizes the element itself from `icon()`, so a hard-coded size
# would only get in the way.
STRIP = (' height="24"', ' width="24"')

# Smallest plausible symbol, used to tell a real fetch from an error page.
MIN_BYTES = 60


def fetch(name: str, dest: Path) -> bool:
    """Download one symbol, retrying: the host rate-limits and drops often."""
    result = subprocess.run(
        [
            "curl", "-sS", "-f",
            "--retry", "6", "--retry-all-errors", "--retry-delay", "2",
            "-o", str(dest), BASE.format(name=name),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0 or not dest.is_file() or dest.stat().st_size < MIN_BYTES:
        dest.unlink(missing_ok=True)
        return False
    text = dest.read_text()
    for attribute in STRIP:
        text = text.replace(attribute, "")
    dest.write_text(text)
    return True


def main() -> int:
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)

    missing = []
    for adb_name, material_name in sorted(MAPPING.items()):
        if not fetch(material_name, OUT / f"{adb_name}.svg"):
            missing.append((adb_name, material_name))

    # Apache-2.0 requires the notice to travel with the files, and the licence
    # itself is what the notice points at.
    here = Path(__file__).resolve().parent.parent / "assets" / "icons"
    for name in ("LICENSE", "NOTICE"):
        if (here / name).is_file():
            shutil.copy(here / name, OUT / name)

    installed = len(MAPPING) - len(missing)
    print(f"installed {installed} icons into {OUT}")
    if missing:
        for adb_name, material_name in missing:
            print(f"missing upstream: {material_name} (for {adb_name})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())