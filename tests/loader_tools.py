"""Puts esp-wifi-loader's tools (sign.py, wl_format.py, upload.py, reset.py, provision.py) on sys.path: this repository does
not carry copies of them. ESP_WIFI_LOADER_DIR, default ../esp-wifi-loader (env.sh sets it)."""
import os
import sys

LOADER = os.environ.get("ESP_WIFI_LOADER_DIR") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "esp-wifi-loader")
TOOLS = os.path.join(LOADER, "tools")
if not os.path.isdir(TOOLS):
    sys.exit(f"esp-wifi-loader not found at {LOADER} (set ESP_WIFI_LOADER_DIR)")
if TOOLS not in sys.path:
    sys.path.insert(0, TOOLS)
