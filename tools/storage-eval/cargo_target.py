"""Where cargo puts this harness's release binaries.

Ask cargo for its target folder instead of assuming one beside the harness:
CARGO_TARGET_DIR or a dev-host build slot can move it.
"""
from pathlib import Path
import json, subprocess

def release_binary(root, name):
    metadata=subprocess.check_output(['cargo','metadata','--format-version','1','--no-deps','--manifest-path',str(root/'Cargo.toml')],text=True)
    return Path(json.loads(metadata)['target_directory'])/'release'/name
