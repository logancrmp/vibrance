#!/usr/bin/env python3
"""Download the campfire scene's photo-scanned assets (all CC0, from Poly Haven).

They go to $XDG_DATA_HOME/vibrance/scenes/campfire (usually ~/.local/share/...), or the
directory given as the first argument. Files already there are skipped.
"""

import json
import os
import sys
import urllib.request
from pathlib import Path

API = "https://api.polyhaven.com/files/"
RES = "4k"
# The sky fills the whole background, so it gets more pixels.
SKY_RES = "8k"

# (asset, kind): models come as glTF with their textures; textures as PBR maps; HDRIs as .hdr.
ASSETS = [
    ("stone_fire_pit", "model"),
    ("dead_tree_trunk", "model"),
    ("dead_tree_trunk_02", "model"),
    ("forrest_ground_03", "texture"),
    ("burned_ground_01", "texture"),
    ("satara_night_no_lamps", "hdri"),
]
TEXTURE_MAPS = {"Diffuse": "diff", "nor_gl": "nor_gl", "arm": "arm"}


def get_json(url):
    request = urllib.request.Request(url, headers={"User-Agent": "vibrance-asset-fetch"})
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def download(url, dest):
    if dest.exists() and dest.stat().st_size > 0:
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    print(f"  {dest.name}")
    request = urllib.request.Request(url, headers={"User-Agent": "vibrance-asset-fetch"})
    tmp = dest.with_suffix(dest.suffix + ".part")
    with urllib.request.urlopen(request) as response, open(tmp, "wb") as out:
        while chunk := response.read(1 << 20):
            out.write(chunk)
    tmp.rename(dest)


def main():
    data_home = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else data_home / "vibrance/scenes/campfire"
    for asset, kind in ASSETS:
        print(asset)
        files = get_json(API + asset)
        folder = root / asset
        if kind == "model":
            gltf = files["gltf"][RES]["gltf"]
            download(gltf["url"], folder / f"{asset}.gltf")
            for rel, item in gltf["include"].items():
                download(item["url"], folder / rel)
        elif kind == "texture":
            for key, name in TEXTURE_MAPS.items():
                download(files[key][RES]["jpg"]["url"], folder / f"{name}.jpg")
        else:
            download(files["hdri"][SKY_RES]["hdr"]["url"], folder / "sky.hdr")
    print(f"done: {root}")


if __name__ == "__main__":
    main()
