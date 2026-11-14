"""Upscale policy shared by the job builder and the batch runner.

Groups are named after what the textures are, not after a size: each group holds sources from 32 px
to 1024 px and above. A group has a maximum output side (`MAX_SIDE`); a texture whose source is
already that large is left alone. How far a texture is enlarged inside the cap depends on its source.
Composite pieces (character atlas) are the exception: fixed slots, so they are scaled by the atlas scale.
"""

MAX_SIDE = {
    "weapon_armor": 2048,    # weapon and armor models
    "character_parts": 2048, # hair, mane, eyes: model textures outside the skin/face atlas
    "world": 1024,           # scenery, objects, creatures, mounts, anything unclassified
    "effect": 1024,          # spell/ambient effects
    "terrain": 1024,         # terrain tiles (ATEP), only the ones the game has been seen loading
}
DEFAULT_MAX_SIDE = 1024
# Fixed-factor groups, never capped: atlas pieces (scaled by the atlas scale) and the 4x variant packs.
PIECE_GROUPS = {"composite", "composite_4x", "character_4x"}


def shipped_ids_path(textures_dir, name):
    """`name` from a copy next to the textures, else the one shipped in the repo (`data/`)."""
    from pathlib import Path

    mine = Path(textures_dir) / name
    return mine if mine.exists() else Path(__file__).resolve().parents[2] / "data" / name


def composite_ids_path(textures_dir):
    """Ids of the character-atlas piece textures (`cmpc` targets and their size twins).

    A copy next to the textures wins (it can be refreshed after a game patch); otherwise the one shipped
    in the repo (`data/composite_ids.txt`) is used.
    """
    return shipped_ids_path(textures_dir, "composite_ids.txt")


def max_side(group: str) -> int:
    return MAX_SIDE.get(group, DEFAULT_MAX_SIDE)


def already_high(job: dict) -> bool:
    """True when the source already reaches its group's maximum side (pieces are always scaled)."""
    if job["group"] in PIECE_GROUPS:
        return False
    return max(job["width"], job["height"]) >= max_side(job["group"])
