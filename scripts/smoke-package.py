import json
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path


root = Path(__file__).resolve().parent.parent
target = sys.argv[1]
manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
extension = ".exe" if target == "windows-x64" else ""
member = f"bin/{target}/dbx-plugin-dbx-plugin-rdp{extension}"
packages = list((root / "dist").glob(f"*-{manifest['version']}-{target}.dbxp"))
if len(packages) != 1:
    raise SystemExit(f"expected 1 .dbxp package, found {len(packages)}")

with zipfile.ZipFile(packages[0]) as package:
    try:
        sidecar = package.getinfo(member)
    except KeyError as error:
        raise SystemExit(f"sidecar missing from package: {member}") from error
    with tempfile.TemporaryDirectory(prefix="dbxp-sidecar-smoke-") as temp_dir:
        executable = Path(temp_dir) / Path(member).name
        executable.write_bytes(package.read(sidecar))
        if target != "windows-x64":
            executable.chmod((sidecar.external_attr >> 16) & 0o777 or 0o755)
        result = subprocess.run(
            ["node", str(root / "scripts" / "smoke-sidecar.mjs"), str(executable)],
            cwd=root,
            check=False,
        )
        raise SystemExit(result.returncode)
