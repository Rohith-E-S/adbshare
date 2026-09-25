#!/usr/bin/env python3
"""Generate the adbshare GPUI icon set (16x16 stroke-based, currentColor)."""
import os
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "assets/icons"

HEAD = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" '
        'stroke="currentColor" stroke-width="1.35" stroke-linecap="round" '
        'stroke-linejoin="round">')
TAIL = "</svg>"

# reusable fragments
FOLDER_BODY = '<path d="M1.9 4.1a1 1 0 0 1 1-1h3.1l1.2 1.5h5.9a1 1 0 0 1 1 1v6.3a1 1 0 0 1-1 1H2.9a1 1 0 0 1-1-1z"/>'
DOC_BODY = '<path d="M4 1.9h4.6L12 5.3v8.8a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V2.9a1 1 0 0 1 1-1z"/><path d="M8.5 2v3.4h3.4"/>'
TRAY = '<path d="M2.4 8.6v3.5a1 1 0 0 0 1 1h9.2a1 1 0 0 0 1-1V8.6"/><path d="M2.4 8.6h2.9l1 1.9h3.4l1-1.9h2.9"/>'
BARS = '<path d="M3.2 4.1h9.6M3.2 8h9.6M3.2 11.9h9.6"/>'
DOWN_ARROW = '<path d="M8 3.2v6.2"/><path d="M5.3 7l2.7 2.6L10.7 7"/>'
UP_ARROW = '<path d="M8 12.8V6.6"/><path d="M5.3 9.2 8 6.6l2.7 2.6"/>'

ICONS = {
    # ── chrome: navigation ───────────────────────────────────────────────
    "sidebar-show": '<path d="M2.4 3.4a1 1 0 0 1 1-1h9.2a1 1 0 0 1 1 1v9.2a1 1 0 0 1-1 1H3.4a1 1 0 0 1-1-1z"/><path d="M6.3 2.4v11.2"/><path d="M3.4 5.4h1.9v5.2H3.4z" fill="currentColor" stroke="none"/>',
    "go-previous": '<path d="M9.9 3.4 5.3 8l4.6 4.6"/>',
    "go-next": '<path d="M6.1 3.4 10.7 8l-4.6 4.6"/>',
    "go-down": '<path d="M3.4 6.1 8 10.7l4.6-4.6"/>',
    "go-up": '<path d="M8 12.6V3.4"/><path d="M4.4 7 8 3.4 11.6 7"/>',
    "window-close": '<path d="M4.2 4.2 11.8 11.8"/><path d="M11.8 4.2 4.2 11.8"/>',

    # ── chrome: menus / view modes ───────────────────────────────────────
    "view-more": ('<path d="M8 4.1h.01M8 8h.01M8 11.9h.01" stroke-width="2.1" '
                  'stroke-linecap="round"/>'),
    "view-refresh": ('<path d="M13.1 6.9A5.2 5.2 0 0 0 4.3 4.7"/>'
                     '<path d="M4.2 2.3v2.5h2.5"/>'
                     '<path d="M2.9 9.1a5.2 5.2 0 0 0 8.8 2.2"/>'
                     '<path d="M11.8 13.7v-2.5H9.3"/>'),
    "view-grid": ('<path d="M2.5 2.5h4.4v4.4H2.5z"/><path d="M9.1 2.5h4.4v4.4H9.1z"/>'
                  '<path d="M2.5 9.1h4.4v4.4H2.5z"/><path d="M9.1 9.1h4.4v4.4H9.1z"/>'),
    "view-list": f'<path d="M6 4.2h7.6M6 8h7.6M6 11.8h7.6"/><path d="M2.6 4.2h.01M2.6 8h.01M2.6 11.8h.01" stroke-width="2.1"/>',
    "edit-find": '<circle cx="7.1" cy="7.1" r="3.9"/><path d="M9.9 9.9 13 13"/>',
    "edit-select-all": '<path d="M2.6 4.6a1 1 0 0 1 1-1h8.8a1 1 0 0 1 1 1v6.8a1 1 0 0 1-1 1H3.6a1 1 0 0 1-1-1z" stroke-dasharray="2 1.9"/>',

    # ── chrome: actions ──────────────────────────────────────────────────
    "document-edit": '<path d="M11.1 2.6 13.4 5 6 12.4l-3 .7.7-3z"/><path d="M9.6 4.1 12 6.5"/>',
    "edit-copy": '<path d="M5.6 5.6h6.9a1 1 0 0 1 1 1v6.9a1 1 0 0 1-1 1H5.6a1 1 0 0 1-1-1V6.6a1 1 0 0 1 1-1z"/><path d="M10.4 5.6V3.4a1 1 0 0 0-1-1H2.5a1 1 0 0 0-1 1v6.9a1 1 0 0 0 1 1h2.1"/>',
    "edit-paste": '<path d="M6.1 2.6H3.5a1 1 0 0 0-1 1v9.9a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1V3.6a1 1 0 0 0-1-1H9.9"/><path d="M6.1 1.2h3.8v2.6H6.1z"/><path d="M5.4 8.1h5.2M5.4 10.7h5.2"/>',
    "edit-delete": '<path d="M2.9 4.3h10.2"/><path d="M6.1 4.3V3.1a.9.9 0 0 1 .9-.9h2a.9.9 0 0 1 .9.9v1.2"/><path d="M4.1 4.3l.6 8.4a1 1 0 0 0 1 .9h4.6a1 1 0 0 0 1-.9l.6-8.4"/><path d="M6.7 6.8v4M9.3 6.8v4"/>',
    "user-trash": '<path d="M2.9 4.3h10.2"/><path d="M6.1 4.3V3.1a.9.9 0 0 1 .9-.9h2a.9.9 0 0 1 .9.9v1.2"/><path d="M4.1 4.3l.6 8.4a1 1 0 0 0 1 .9h4.6a1 1 0 0 0 1-.9l.6-8.4"/>',
    "edit-undo": '<path d="M3 6.6h6.4a3.4 3.4 0 0 1 0 6.8H6.1"/><path d="M5.4 4 2.8 6.6l2.6 2.6"/>',

    # ── places ───────────────────────────────────────────────────────────
    "phone": '<path d="M5.1 1.9h5.8a1 1 0 0 1 1 1v10.2a1 1 0 0 1-1 1H5.1a1 1 0 0 1-1-1V2.9a1 1 0 0 1 1-1z"/><path d="M6.9 12.2h2.2"/><path d="M6.7 4.2h2.6"/>',
    "user-home": '<path d="M2.4 7.4 8 2.4l5.6 5"/><path d="M4 6.8v6.8h8V6.8"/><path d="M6.6 13.6V9.5h2.8v4.1"/>',
    "drive-harddisk": '<path d="M2.6 4.4a1 1 0 0 1 1-1h8.8a1 1 0 0 1 1 1v7.2a1 1 0 0 1-1 1H3.6a1 1 0 0 1-1-1z"/><circle cx="10.6" cy="10.6" r="1"/><path d="M4.4 3.4v9.2"/>',
    "system-file-manager": '<path d="M2.5 4.4a1 1 0 0 1 1-1h9a1 1 0 0 1 1 1v7.2a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1z"/><path d="M2.5 8.4h11"/><path d="M4.6 5.2h6.8"/>',

    # ── folders ──────────────────────────────────────────────────────────
    "folder": FOLDER_BODY,
    "folder-new": FOLDER_BODY + '<path d="M8 7.3v3.6M6.2 9.1h3.6"/>',
    "folder-open": '<path d="M1.9 12.1V4.1a1 1 0 0 1 1-1h3.1l1.2 1.5h5.9a1 1 0 0 1 1 1v1.2"/><path d="M1.9 12.1l1.8-5.2a1 1 0 0 1 .9-.7h9.7a.6.6 0 0 1 .6.8l-1.7 4.6a1 1 0 0 1-.9.7H1.9z"/>',
    "folder-download": FOLDER_BODY + DOWN_ARROW,
    "folder-upload": FOLDER_BODY + UP_ARROW,
    "folder-documents": FOLDER_BODY + '<path d="M6.1 7.9h3.8M6.1 9.7h3.8M6.1 11.5h2.4"/>',
    "folder-music": FOLDER_BODY + '<path d="M10.9 7.6v3.6"/><circle cx="9.7" cy="11.2" r="1.1"/><path d="M10.9 7.6 7.9 8.4v3"/><circle cx="6.7" cy="11.4" r="1.1"/>',
    "folder-pictures": FOLDER_BODY + '<rect x="6" y="8.2" width="4.6" height="3.5" rx=".5"/><path d="M6.4 11.2 7.7 9.9l1.1 1.1.9-.8.9 1"/>',
    "folder-videos": FOLDER_BODY + '<path d="M6.4 8.3 10.7 10 6.4 11.7z"/>',
    "folder-copy": '<path d="M1.9 9.8V4.1a1 1 0 0 1 1-1h3.1l1.1 1.4h3.2"/><path d="M1.9 9.8l1.3-3.9h10.2a.6.6 0 0 1 .6.7l-1.4 4.3a1 1 0 0 1-.9.7H2.9a1 1 0 0 1-1-1z"/><path d="M4.6 9.8a1 1 0 0 0 1 1h7.9l-1.4 4.3a1 1 0 0 1-.9.7H3.6a1 1 0 0 1-1-1V9.8z"/>',
    "folder-send": FOLDER_BODY + '<path d="M10.4 8.2 12 9.8l-1.6 1.6"/><path d="M11.7 9.8H8.3"/>',

    # ── documents / file types ───────────────────────────────────────────
    "x-office-document": DOC_BODY,
    "text-x-generic": DOC_BODY + '<path d="M5.4 8.1h4.2M5.4 10.3h4.2M5.4 12.5h2.6"/>',
    "image-x-generic": '<rect x="2.1" y="3.1" width="11.8" height="9.8" rx="1"/><circle cx="5.8" cy="6.3" r="1.1"/><path d="M2.6 10.4 5.6 7.8l2.1 1.9 2-1.7 3.7 3.1"/>',
    "audio-x-generic": '<path d="M13 2.4 6.6 3.8v6.9"/><path d="M13 2.4v6.3"/><circle cx="4.9" cy="10.7" r="1.8"/><circle cx="11.3" cy="8.7" r="1.8"/>',
    "video-x-generic": '<rect x="1.8" y="3.6" width="12.4" height="8.8" rx="1"/><path d="M1.8 6.2h12.4M1.8 9.8h12.4"/><path d="M4.2 3.6v2.6M7 3.6v2.6M9.8 3.6v2.6M12.4 3.6v2.6M4.2 9.8v2.6M7 9.8v2.6M9.8 9.8v2.6M12.4 9.8v2.6"/>',
    "package-x-generic": '<path d="M8 1.7 14.1 5v6L8 14.3 1.9 11V5z"/><path d="M1.9 5 8 8.3 14.1 5M8 8.3v6"/>',
    "application-x-executable": '<rect x="2.1" y="2.7" width="11.8" height="10.6" rx="1.1"/><path d="M5.1 6.4 7 8l-1.9 1.6"/><path d="M8.6 9.9h2.6"/>',
    "camera-photo": '<path d="M2.1 4.9a1 1 0 0 1 1-1h1.9l1-1.5h3a1 1 0 0 1 1 1.5h1.9a1 1 0 0 1 1 1v6.2a1 1 0 0 1-1 1H3.1a1 1 0 0 1-1-1z"/><circle cx="8" cy="7.9" r="2.3"/>',

    # ── transfer / media control ─────────────────────────────────────────
    "emblem-synchronizing": '<path d="M13.2 7.2a5.2 5.2 0 0 0-9-3.1L2.6 5.7"/><path d="M2.8 8.8a5.2 5.2 0 0 0 9 3.1l1.6-1.6"/><path d="M2.6 2.9v2.8h2.8M13.4 13.1v-2.8h-2.8"/>',
    "media-playback-pause": '<path d="M5.4 3.6v8.8M10.6 3.6v8.8" stroke-width="2"/>',
    "media-playback-start": '<path d="M5.2 3.4 12.4 8l-7.2 4.6z"/>',
    "process-stop": '<rect x="3.6" y="3.6" width="8.8" height="8.8" rx="1"/>',
    "view-continuous": '<path d="M2.2 8h3.4l2-4.4 2.4 8.8 2-4.4h2.2"/>',

    # ── system integration ───────────────────────────────────────────────
    "network-wireless": '<path d="M1.6 5.9a9 9 0 0 1 12.8 0"/><path d="M4.1 8.4a5.7 5.7 0 0 1 7.8 0"/><path d="M6.5 10.8a2.5 2.5 0 0 1 3 0"/><path d="M8 13.1h.01" stroke-width="2.1"/>',
    "utilities-terminal": '<rect x="1.8" y="2.7" width="12.4" height="10.6" rx="1.1"/><path d="M4.4 6.3 6.4 8l-2 1.7"/><path d="M7.8 9.9h3.4"/>',
    "document-open": '<path d="M8.6 2.1H4a1 1 0 0 0-1 1v9.8a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V6.5z"/><path d="M8.6 2.1v4.4H13"/>',
    "document-send": '<path d="M14 2 2.2 6.3l4.3 2 2 4.3z"/><path d="M14 2 6.5 8.3"/>',
    "send-to": '<path d="M14 2 2.2 6.3l4.3 2 2 4.3z"/><path d="M14 2 6.5 8.3"/>',
    "system-software-install": '<path d="M8 2.4v7.2"/><path d="M5.3 6.9 8 9.6l2.7-2.7"/><path d="M2.4 9.6v2.6a1 1 0 0 0 1 1h9.2a1 1 0 0 0 1-1V9.6"/>',
    "system-lock": '<rect x="3.4" y="7" width="9.2" height="6.4" rx="1"/><path d="M5.6 7V5.2a2.4 2.4 0 0 1 4.8 0V7"/>',
    "emblem-symbolic-link": '<path d="M6.6 9.4a2.7 2.7 0 0 0 4 .3l1.8-1.8a2.7 2.7 0 0 0-3.8-3.8l-1 1"/><path d="M9.4 6.6a2.7 2.7 0 0 0-4-.3L3.6 8.1a2.7 2.7 0 0 0 3.8 3.8l1-1"/>',

    # ── info / help ──────────────────────────────────────────────────────
    "dialog-information": '<circle cx="8" cy="8" r="6"/><path d="M8 7.4v3.6"/><path d="M8 5.2h.01" stroke-width="2.1"/>',
    "help-about": '<circle cx="8" cy="8" r="6"/><path d="M6.3 6.2a1.8 1.8 0 0 1 3.4.8c0 1.2-1.7 1.5-1.7 2.7"/><path d="M8 11.4h.01" stroke-width="2.1"/>',
    "dialog-warning": '<path d="M8 2.4 14.4 13H1.6z"/><path d="M8 6.4v3.2"/><path d="M8 11.3h.01" stroke-width="2.1"/>',
    "dialog-error": '<circle cx="8" cy="8" r="6"/><path d="M5.9 5.9l4.2 4.2M10.1 5.9l-4.2 4.2"/>',

    # ── check / chevrons ─────────────────────────────────────────────────
    "object-select": '<path d="M2.6 8.4 6.1 12l7.3-8"/>',
    "go-down-bold": '<path d="M8 3v10M3.6 8.6 8 13l4.4-4.4"/>',
    "checkbox-checked": '<path d="M2.4 4.6a1 1 0 0 1 1-1h9.2a1 1 0 0 1 1 1v6.8a1 1 0 0 1-1 1H3.4a1 1 0 0 1-1-1z"/><path d="M4.7 8.1 6.9 10.3l4.4-4.6" stroke="currentColor"/>',
}

os.makedirs(OUT, exist_ok=True)
for name, body in ICONS.items():
    with open(os.path.join(OUT, f"{name}.svg"), "w") as fh:
        fh.write(HEAD + body + TAIL)
print(f"wrote {len(ICONS)} icons to {OUT}")
